//! Search-only evidence snapshots. Does not observe ingest, extraction, channel
//! truncation, or context packing by the caller. Traced requests bypass cache;
//! async reranking is consequently not observed. Tier-1 does not emit this block.
use super::{filter_candidates, AppState, FilterOutcome, QueryContext, SearchRequest};
use crate::handlers::types::{EvidenceStage, EvidenceTrace};
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
    });
    capture(&mut trace, "ranked_candidates", &candidates, ctx, state);
    // Preserve the existing defense-in-depth filter before the external reranker.
    let candidates: Vec<_> = candidates
        .into_iter()
        .filter(|r| !crate::handlers::delete::is_deleted_in_store(state, &r.id))
        .collect();
    capture(&mut trace, "after_soft_delete", &candidates, ctx, state);
    let outcome = filter_candidates(candidates, ctx, payload, state);
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
    (outcome, trace)
}

fn capture(
    trace: &mut Option<EvidenceTrace>,
    stage: &'static str,
    candidates: &[HybridSearchResult],
    ctx: &QueryContext,
    state: &Arc<AppState>,
) {
    let Some(trace) = trace else { return };
    // Even diagnostic IDs must not expose a different stream. Missing or
    // unreadable chunks are omitted from snapshots, not inferred as evidence.
    let chunk_ids = candidates
        .iter()
        .filter(|r| {
            matches!(state.store.get_chunk(&r.id), Ok(Some(chunk))
                if ctx.stream_list.as_ref().is_none_or(|s| s.contains(&chunk.stream)))
        })
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

    fn seed(state: &Arc<AppState>, id: &str, stream: &str) -> HybridSearchResult {
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
        let serialized = serde_json::to_string(&trace).unwrap();
        assert!(!serialized.contains(&payload.query));
        assert!(!serialized.contains("independent"));
        assert!(!serialized.contains("foreign"));
        let mut disabled = payload;
        disabled.trace = false;
        let (plain, absent) = filter_with_trace(candidates, &ctx, &disabled, &state);
        assert!(absent.is_none());
        assert_eq!(
            plain.results.iter().map(|r| &r.id).collect::<Vec<_>>(),
            outcome.results.iter().map(|r| &r.id).collect::<Vec<_>>()
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
                assert_eq!(evidence["stages"][3]["chunk_ids"], json!([]));
                assert!(!evidence.to_string().contains("private-canary"));
            }
        }
    }
}
