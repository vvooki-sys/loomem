//! Authoritative event-date admission for versioned ingest, independent of stale indexes.
use loomem_core::storage::{is_ingest_fact_shape, is_ingest_source, Chunk};
use loomem_core::{tantivy_index::event_timestamp_for_chunk, HybridSearchResult};
use std::collections::HashSet;
use std::sync::Arc;

use super::{DateFilter, QueryContext, SearchRequest};
use crate::AppState;

pub(super) fn allows(chunk: &Chunk, filter: Option<&DateFilter>) -> bool {
    let Some(DateFilter::Range(start, end)) = filter else {
        return true;
    };
    if !is_ingest_source(chunk) && !is_ingest_fact_shape(chunk) {
        return true;
    }
    event_timestamp_for_chunk(chunk).is_some_and(|event| (*start..=*end).contains(&event))
}

pub(super) fn allows_id(id: &str, filter: Option<&DateFilter>, state: &AppState) -> bool {
    filter.is_none()
        || state
            .store
            .get_chunk(id)
            .ok()
            .flatten()
            .is_some_and(|chunk| allows(&chunk, filter))
}

pub(super) fn embeddings(
    mut embeddings: Vec<(String, Vec<f32>)>,
    filter: Option<&DateFilter>,
    state: &AppState,
) -> Vec<(String, Vec<f32>)> {
    embeddings.retain(|(id, _)| allows_id(id, filter, state));
    embeddings
}

/// Apply authoritative dates to lexical inputs before the fusion candidate cap.
pub(super) async fn inputs(
    state: &Arc<AppState>,
    payload: &SearchRequest,
    ctx: &QueryContext,
    bm25: Vec<loomem_core::SearchResult>,
) -> (Option<HashSet<String>>, Vec<loomem_core::SearchResult>) {
    let agents = super::agent_id_set(state, payload).await;
    let bm25 = bm25
        .into_iter()
        .filter(|result| allows_id(&result.id, ctx.date_filter.as_ref(), state))
        .collect();
    (agents, bm25)
}

pub(super) fn multi_query_embeddings(
    state: &AppState,
    ctx: &QueryContext,
    agents: Option<&HashSet<String>>,
) -> Vec<(String, Vec<f32>)> {
    let all =
        super::retain_by_agent_set(state.store.get_all_embeddings().unwrap_or_default(), agents);
    embeddings(all, ctx.date_filter.as_ref(), state)
}

/// Keep event-date semantics separate from the existing bitemporal valid_at scope.
/// `log` records each dropped id with its reason (inert unless tracing).
pub(super) fn apply(
    results: &mut Vec<HybridSearchResult>,
    ctx: &QueryContext,
    payload: &SearchRequest,
    state: &AppState,
    log: &mut super::rejections::RejectionLog,
) {
    use crate::handlers::types::RejectionReason;
    let stage = super::rejections::STAGE_FILTERS;
    log.retain(
        results,
        stage,
        RejectionReason::EventDateOutOfRange,
        |result| allows_id(&result.id, ctx.date_filter.as_ref(), state),
    );
    // Preserve existing metadata-load failure policies for validity and supersession.
    if let Some(valid_at) = payload.valid_at {
        let before = log.snapshot(results);
        super::filter_bitemporal(results, &state.store, valid_at);
        log.record_dropped(
            before,
            results,
            stage,
            RejectionReason::BitemporalOutOfRange,
        );
    }
    if !payload.include_superseded {
        let before = log.snapshot(results);
        super::filter_superseded(results, &state.store);
        log.record_dropped(before, results, stage, RejectionReason::Superseded);
    }
}

/// A supplemental operation lane cannot reintroduce a stale indexed event date.
pub(super) fn finish(
    lane: super::operation_lane::LaneOutcome,
    metrics: super::telemetry::ResponseMetrics,
    ctx: &QueryContext,
    state: &AppState,
) -> (Vec<HybridSearchResult>, super::telemetry::ResponseMetrics) {
    let (mut results, metrics) = lane.finish(metrics);
    results.retain(|result| allows_id(&result.id, ctx.date_filter.as_ref(), state));
    (results, metrics)
}

#[cfg(test)]
mod tests;
