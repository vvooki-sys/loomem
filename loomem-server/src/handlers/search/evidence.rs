//! Search-only evidence snapshots. Does not observe ingest, extraction, channel
//! truncation, or context packing by the caller. Traced requests bypass cache;
//! async reranking is consequently not observed. Tier-1 does not emit this block.
use super::rejections::{RejectionLog, STAGE_SOFT_DELETE};
use super::{filter_candidates, AppState, FilterOutcome, QueryContext, SearchRequest};
use crate::handlers::types::{EvidenceStage, EvidenceTrace, RejectionReason};
use loomem_core::HybridSearchResult;
use std::sync::Arc;

pub(super) fn filter_with_trace(
    candidates: Vec<HybridSearchResult>,
    ctx: &QueryContext,
    payload: &SearchRequest,
    state: &Arc<AppState>,
) -> (FilterOutcome, Option<EvidenceTrace>) {
    let mut trace = payload.trace.then(|| EvidenceTrace {
        schema_version: 1,
        coverage: "post_rank_to_response",
        stages: Vec::new(),
        rejections: Vec::new(),
    });
    let mut log = RejectionLog::new(payload.trace);
    capture(&mut trace, "ranked_candidates", &candidates, ctx, state);
    // Preserve the existing defense-in-depth filter before the external reranker.
    let mut candidates = candidates;
    log.retain(
        &mut candidates,
        STAGE_SOFT_DELETE,
        RejectionReason::SoftDeleted,
        |r| !crate::handlers::delete::is_deleted_in_store(state, &r.id),
    );
    capture(&mut trace, "after_soft_delete", &candidates, ctx, state);
    let mut outcome = filter_candidates(candidates, ctx, payload, state);
    capture(
        &mut trace,
        "after_filters_and_dedup",
        &outcome.results,
        ctx,
        state,
    );
    capture(
        &mut trace,
        "response",
        &outcome.results[..outcome.final_top_k.min(outcome.results.len())],
        ctx,
        state,
    );
    log.append(std::mem::take(&mut outcome.rejections));
    log.record_truncated(&outcome.results, outcome.final_top_k);
    if let Some(trace) = trace.as_mut() {
        trace.rejections = log
            .into_entries()
            .into_iter()
            .filter(|rejection| in_scope(&rejection.chunk_id, ctx, state))
            .collect();
    }
    (outcome, trace)
}

/// Observe the final lane and authoritative date pass without changing either.
pub(super) fn finish(
    lane: super::operation_lane::LaneOutcome,
    mut metrics: super::telemetry::ResponseMetrics,
    ctx: &QueryContext,
    state: &Arc<AppState>,
) -> (Vec<HybridSearchResult>, super::telemetry::ResponseMetrics) {
    capture(
        &mut metrics.evidence,
        "after_operation",
        &lane.results,
        ctx,
        state,
    );
    let (results, mut metrics) = super::event_date::finish(lane, metrics, ctx, state);
    reconcile(&mut metrics.evidence, &results, ctx, state);
    // The temporary lane snapshot is internal. Preserve the v1 four-stage
    // contract and replace its provisional response only after every final pass.
    if let Some(trace) = metrics.evidence.as_mut() {
        trace.stages.truncate(3);
    }
    capture(&mut metrics.evidence, "response", &results, ctx, state);
    (results, metrics)
}

fn reconcile(
    trace: &mut Option<EvidenceTrace>,
    results: &[HybridSearchResult],
    ctx: &QueryContext,
    state: &Arc<AppState>,
) {
    let Some(trace) = trace else { return };
    let returned = |id: &str| results.iter().any(|result| result.id == id);
    // A lane may restore a candidate previously filtered or cut by top-k.
    // Historical snapshots preserve that loss; terminal rejections must not lie.
    let admitted: Vec<_> = trace
        .stages
        .iter()
        .filter(|stage| stage.stage == "after_operation")
        .flat_map(|stage| stage.chunk_ids.iter().cloned())
        .collect();
    trace
        .rejections
        .retain(|entry| !returned(&entry.chunk_id) && !admitted.contains(&entry.chunk_id));
    record_stage_loss(
        trace,
        "response",
        "after_operation",
        RejectionReason::OperationFinalization,
    );
    let rejected: Vec<_> = trace
        .stages
        .iter()
        .filter(|stage| stage.stage == "after_operation")
        .flat_map(|stage| &stage.chunk_ids)
        .filter(|id| !returned(id) && in_scope(id, ctx, state))
        .cloned()
        .collect();
    trace
        .rejections
        .extend(
            rejected
                .into_iter()
                .map(|chunk_id| crate::handlers::types::Rejection {
                    chunk_id,
                    stage: "response",
                    reason: RejectionReason::EventDateOutOfRange,
                }),
        );
}

fn record_stage_loss(
    trace: &mut EvidenceTrace,
    before: &str,
    after: &'static str,
    reason: RejectionReason,
) {
    let ids = |name| {
        trace
            .stages
            .iter()
            .find(|stage| stage.stage == name)
            .map(|stage| stage.chunk_ids.clone())
            .unwrap_or_default()
    };
    let retained = ids(after);
    let dropped: Vec<_> = ids(before)
        .into_iter()
        .filter(|id| !retained.contains(id))
        .collect();
    trace.rejections.extend(dropped.into_iter().map(|chunk_id| {
        crate::handlers::types::Rejection {
            chunk_id,
            stage: "response",
            reason,
        }
    }));
}

/// Even diagnostic IDs must not expose a different stream. Missing or
/// unreadable chunks are omitted from snapshots, not inferred as evidence.
fn in_scope(id: &str, ctx: &QueryContext, state: &Arc<AppState>) -> bool {
    matches!(state.store.get_chunk(id), Ok(Some(chunk))
        if ctx.stream_list.as_ref().is_none_or(|s| s.contains(&chunk.stream)))
}

fn capture(
    trace: &mut Option<EvidenceTrace>,
    stage: &'static str,
    candidates: &[HybridSearchResult],
    ctx: &QueryContext,
    state: &Arc<AppState>,
) {
    let Some(trace) = trace else { return };
    let chunk_ids = candidates
        .iter()
        .filter(|r| in_scope(&r.id, ctx, state))
        .map(|r| r.id.clone())
        .collect();
    trace.stages.push(EvidenceStage { stage, chunk_ids });
}

pub(super) fn emit_search_event(
    state: &Arc<AppState>,
    auth: &crate::auth::AuthContext,
    results: &[crate::handlers::types::SearchResult],
    took_ms: u64,
) {
    if let Some(ref tx) = state.event_tx {
        // truncation intentional: retain the existing f32 telemetry score representation.
        let top_scores: Vec<f32> = results.iter().take(5).map(|r| r.score as f32).collect();
        loomem_core::event_log::emit(
            tx,
            loomem_core::event_log::MemoryEvent::Search {
                query: String::new(), // Metadata-only telemetry; never persist query text.
                stream_id: auth.stream_id.clone(),
                top_scores,
                latency_ms: took_ms,
                result_count: results.len(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthContext, KeyScope};
    use loomem_core::storage::{Chunk, UserRole};
    use serde_json::json;

    pub(super) fn seed(state: &Arc<AppState>, id: &str, stream: &str) -> HybridSearchResult {
        let mut chunk = Chunk {
            id: id.to_string(),
            content: format!("independent {id}"),
            stream: stream.to_string(),
            level: 0,
            score: 1.0,
            timestamp: 1_700_000_000,
            consolidated: false,
            dormant: false,
            in_progress: false,
            prompt_version: None,
            source_ids: None,
            last_decay: None,
            metadata: None,
            importance: None,
            persistent: false,
            last_implicit_boost: None,
            access_count: 0,
            source: None,
            created_by: None,
            updated_at: None,
            valid_from: None,
            valid_until: None,
            is_latest: true,
            superseded_by: None,
            supersedes_id: None,
            root_memory_id: None,
            version: 1,
            memory_type: None,
            extraction_meta: None,
            deleted_at: None,
            trust_level: None,
            ingester_user_id: None,
            alpha: 1.0,
            beta: 1.0,
            harmful_count: 0,
            n_ratings: 0,
            last_rated_at: None,
            provenance_role: loomem_core::storage::ProvenanceRole::Claim,
            fragment: None,
        };
        chunk.is_latest = id != "superseded";
        chunk.deleted_at = (id == "deleted").then_some(1);
        state.store.store_chunk(&chunk).unwrap();
        HybridSearchResult {
            id: id.into(),
            content: chunk.content,
            user_id: String::new(),
            app_id: String::new(),
            level: 0,
            timestamp: 0,
            score: 1.0,
            bm25_score: 1.0,
            vector_score: 0.0,
            time_decay_factor: 1.0,
        }
    }

    #[test]
    fn trace_has_ordered_stage_loss_without_query_or_cross_stream_ids() {
        let (_, state) = crate::tests::make_test_app();
        let payload: SearchRequest = serde_json::from_value(json!({
            "query": "private-canary@example.invalid", "trace": true, "top_k": 1,
            "stream": "test", "dry_run": true
        }))
        .unwrap();
        let auth =
            AuthContext::single_stream("test", UserRole::Admin, KeyScope::Shared, None, true);
        let ctx = super::super::prepare_query_context(&state, &auth, &payload).unwrap();
        let candidates = vec![
            seed(&state, "first", "test"),
            seed(&state, "superseded", "test"),
            seed(&state, "deleted", "test"),
            seed(&state, "last", "test"),
            seed(&state, "foreign", "other"),
        ];
        let (outcome, trace) = filter_with_trace(candidates.clone(), &ctx, &payload, &state);
        let trace = trace.unwrap();
        assert_eq!(
            trace.stages[0].chunk_ids,
            ["first", "superseded", "deleted", "last"]
        );
        assert_eq!(trace.stages[1].chunk_ids, ["first", "superseded", "last"]);
        assert_eq!(trace.stages[2].chunk_ids, ["first", "last"]);
        assert_eq!(trace.stages[3].chunk_ids, ["first"]);
        let rejected: Vec<_> = trace
            .rejections
            .iter()
            .map(|r| (r.chunk_id.as_str(), r.stage, r.reason))
            .collect();
        assert_eq!(
            rejected,
            [
                ("deleted", "after_soft_delete", RejectionReason::SoftDeleted),
                (
                    "superseded",
                    "after_filters_and_dedup",
                    RejectionReason::Superseded
                ),
                ("last", "response", RejectionReason::TruncatedTopK),
            ]
        );
        let serialized = serde_json::to_string(&trace).unwrap();
        assert!(!serialized.contains(&payload.query));
        assert!(!serialized.contains("independent"));
        assert!(!serialized.contains("foreign"));
        assert!(
            !serialized.contains("rejections"),
            "lifted by TraceMetadata"
        );
        let mut disabled = payload;
        disabled.trace = false;
        let (plain, absent) = filter_with_trace(candidates, &ctx, &disabled, &state);
        assert!(absent.is_none());
        assert!(plain.rejections.is_empty());
        assert_eq!(
            plain.results.iter().map(|r| &r.id).collect::<Vec<_>>(),
            outcome.results.iter().map(|r| &r.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn rejections_name_dedup_and_filter_reasons() {
        let (_, state) = crate::tests::make_test_app();
        let payload: SearchRequest = serde_json::from_value(json!({
            "query": "q", "trace": true, "top_k": 10, "stream": "test", "dry_run": true,
            "min_confidence": 0.5
        }))
        .unwrap();
        let auth =
            AuthContext::single_stream("test", UserRole::Admin, KeyScope::Shared, None, true);
        let ctx = super::super::prepare_query_context(&state, &auth, &payload).unwrap();
        let mut candidates = vec![
            seed(&state, "twin-a", "test"),
            seed(&state, "twin-b", "test"),
            seed(&state, "weak", "test"),
        ];
        // Near-identical long content: the second twin is dominated by the
        // first (>80% shared 150-char prefix).
        for result in candidates.iter_mut().take(2) {
            result.content = format!("{} tail {}", "same prefix ".repeat(20), result.id);
        }
        for result in &candidates {
            let mut chunk = state.store.get_chunk(&result.id).unwrap().unwrap();
            chunk.content = result.content.clone();
            chunk.extraction_meta = Some(loomem_core::storage::ExtractionMeta {
                fact_type: loomem_core::storage::FactType::Fact,
                subject: None,
                event_date: None,
                event_date_context: None,
                supersedes: None,
                superseded_by: None,
                confidence: if result.id == "weak" { 0.2 } else { 0.9 },
                extracted_from: None,
                extraction_model: None,
                original_content: None,
                topic: None,
                attributed_to: None,
            });
            state.store.store_chunk(&chunk).unwrap();
        }
        let (outcome, trace) = filter_with_trace(candidates, &ctx, &payload, &state);
        assert_eq!(
            outcome
                .results
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["twin-a"]
        );
        let trace = trace.unwrap();
        let rejected: Vec<_> = trace
            .rejections
            .iter()
            .map(|r| (r.chunk_id.as_str(), r.stage, r.reason))
            .collect();
        assert_eq!(
            rejected,
            [
                (
                    "weak",
                    "after_filters_and_dedup",
                    RejectionReason::MinConfidence
                ),
                (
                    "twin-b",
                    "after_filters_and_dedup",
                    RejectionReason::ContentPrefixDedup
                ),
            ]
        );
    }
    #[tokio::test]
    async fn handler_serializes_evidence_only_when_requested() {
        let (_, state) = crate::tests::make_test_app();
        for trace in [false, true] {
            let payload: SearchRequest = serde_json::from_value(json!({
                "query": "private-canary@example.invalid", "trace": trace, "dry_run": true
            }))
            .unwrap();
            let auth =
                AuthContext::single_stream("test", UserRole::Admin, KeyScope::Shared, None, true);
            let response = super::super::search_handler(
                axum::extract::State(state.clone()),
                axum::Extension(auth),
                axum::Json(payload),
            )
            .await
            .unwrap()
            .0;
            let value = serde_json::to_value(response).unwrap();
            assert_eq!(value.get("trace_metadata").is_some(), trace);
            if trace {
                let evidence = &value["trace_metadata"]["evidence"];
                assert_eq!(evidence["schema_version"], 1);
                assert_eq!(
                    evidence["stages"].as_array().unwrap().last().unwrap()["chunk_ids"],
                    json!([])
                );
                assert!(!evidence.to_string().contains("private-canary"));
                assert!(evidence.get("rejections").is_none());
                assert_eq!(value["trace_metadata"]["rejections"], json!([]));
            }
        }
    }
}

#[cfg(test)]
#[path = "evidence/final_tests.rs"]
mod final_tests;
