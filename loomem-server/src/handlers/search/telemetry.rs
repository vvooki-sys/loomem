use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use super::{
    bm25_leaf, evidence, operation, Bm25Leaf, FilterOutcome, QueryComplexity, QueryContext,
};
use crate::handlers::types::{EvidenceTrace, OperationTelemetry, SearchRequest, TraceMetadata};
use crate::AppState;

pub(super) struct ResponseMetrics {
    pub(super) start: Instant,
    pub(super) complexity: QueryComplexity,
    pub(super) dedup_removed: usize,
    pub(super) total_results_before_topk: usize,
    pub(super) final_top_k: usize,
    pub(super) evidence: Option<EvidenceTrace>,
    pub(super) operation: Option<OperationTelemetry>,
}

pub(super) struct TraceCounters {
    pub(super) total_results_before_topk: usize,
    pub(super) dedup_removed: usize,
    pub(super) evidence: Option<EvidenceTrace>,
    pub(super) operation: Option<OperationTelemetry>,
}

impl ResponseMetrics {
    pub(super) fn full(
        ctx: &QueryContext,
        outcome: &FilterOutcome,
        evidence: Option<EvidenceTrace>,
        operation_metrics: operation::OperationMetrics,
    ) -> Self {
        Self {
            start: ctx.start,
            complexity: ctx.complexity,
            dedup_removed: outcome.dedup_removed,
            total_results_before_topk: outcome.total_results_before_topk,
            final_top_k: outcome.final_top_k,
            evidence,
            operation: ctx
                .telemetry
                .as_deref()
                .map(|meter| meter.finish(&ctx.operation, operation_metrics)),
        }
    }

    pub(super) fn tier1(ctx: &QueryContext, outcome: &FilterOutcome) -> Self {
        Self::full(ctx, outcome, None, operation::OperationMetrics::default())
    }
}

#[derive(Default)]
pub(super) struct RequestTelemetry {
    classify_plan_us: AtomicU64,
    expansion_us: AtomicU64,
    bm25_us: AtomicU64,
    pub(super) embedding_vector_us: AtomicU64,
    pub(super) merge_us: AtomicU64,
    final_filter_us: AtomicU64,
    pub(super) rerank_us: AtomicU64,
    bm25_leaf_queries: AtomicUsize,
    embedding_calls: AtomicUsize,
    extra_embedding_calls: AtomicUsize,
    operation_bpe_calls: AtomicUsize,
}

impl RequestTelemetry {
    fn record(slot: &AtomicU64, started: Instant) {
        let elapsed = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        let _ = slot.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(current.saturating_add(elapsed))
        });
    }

    pub(super) fn record_embedding_attempt(&self) {
        self.embedding_calls.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_extra_embedding_attempt(&self) {
        self.record_embedding_attempt();
        self.extra_embedding_calls.fetch_add(1, Ordering::Relaxed);
    }

    fn record_bm25_leaf(&self) {
        self.bm25_leaf_queries.fetch_add(1, Ordering::Relaxed);
    }

    fn finish(
        &self,
        prepared: &operation::PreparedOperation,
        metrics: operation::OperationMetrics,
    ) -> OperationTelemetry {
        let embeddings = self.embedding_calls.load(Ordering::Relaxed);
        OperationTelemetry {
            classify_plan_ms: micros_to_ms(self.classify_plan_us.load(Ordering::Relaxed)),
            expansion_scan_rerank_ms: micros_to_ms(
                self.expansion_us
                    .load(Ordering::Relaxed)
                    .saturating_add(u64::try_from(metrics.elapsed_us).unwrap_or(u64::MAX)),
            ),
            bm25_ms: micros_to_ms(self.bm25_us.load(Ordering::Relaxed)),
            embedding_vector_ms: micros_to_ms(self.embedding_vector_us.load(Ordering::Relaxed)),
            merge_ms: micros_to_ms(self.merge_us.load(Ordering::Relaxed)),
            rerank_ms: micros_to_ms(self.rerank_us.load(Ordering::Relaxed)),
            final_filter_ms: micros_to_ms(self.final_filter_us.load(Ordering::Relaxed)),
            bm25_leaf_queries: self.bm25_leaf_queries.load(Ordering::Relaxed),
            embedding_calls: embeddings,
            extra_embeddings: self.extra_embedding_calls.load(Ordering::Relaxed),
            operation_bpe_calls: self.operation_bpe_calls.load(Ordering::Relaxed),
            scanned_candidates: metrics.scanned_candidates,
            changed_admissions: metrics.changed_admissions,
            scanned_bytes: metrics.scanned_bytes,
            temporary_bytes: metrics.temporary_bytes,
            fallback_to_e0: metrics.fallback.is_some() || prepared.fallback_reason().is_some(),
            operation_kind: prepared.plan.kind(),
            operation_plan_sha256: prepared.plan.identity_sha256(),
        }
    }
}

fn micros_to_ms(value: u64) -> f64 {
    std::time::Duration::from_micros(value).as_secs_f64() * 1_000.0
}

pub(super) struct StageTimer<'a> {
    slot: &'a AtomicU64,
    started: Instant,
}

impl<'a> StageTimer<'a> {
    fn new(slot: &'a AtomicU64) -> Self {
        Self {
            slot,
            started: Instant::now(),
        }
    }
}

impl Drop for StageTimer<'_> {
    fn drop(&mut self) {
        RequestTelemetry::record(self.slot, self.started);
    }
}

pub(super) fn stage_timer(slot: Option<&AtomicU64>) -> Option<StageTimer<'_>> {
    slot.map(StageTimer::new)
}

pub(super) fn embedding_vector_timer(
    telemetry: Option<&RequestTelemetry>,
) -> Option<StageTimer<'_>> {
    stage_timer(telemetry.map(|meter| &meter.embedding_vector_us))
}

pub(super) fn request_telemetry(payload: &SearchRequest) -> Option<Arc<RequestTelemetry>> {
    payload
        .debug_operation
        .then(|| Arc::new(RequestTelemetry::default()))
}

pub(super) fn record_prepare_stage(
    telemetry: Option<&RequestTelemetry>,
    started: Instant,
    expansion: bool,
) {
    let Some(meter) = telemetry else {
        return;
    };
    let slot = if expansion {
        &meter.expansion_us
    } else {
        &meter.classify_plan_us
    };
    RequestTelemetry::record(slot, started);
}

pub(super) struct Bm25Runner<'a> {
    pub(super) tantivy: &'a loomem_core::TantivyIndex,
    pub(super) payload: &'a SearchRequest,
    pub(super) telemetry: Option<&'a RequestTelemetry>,
}

impl Bm25Runner<'_> {
    pub(super) fn new<'a>(
        tantivy: &'a loomem_core::TantivyIndex,
        payload: &'a SearchRequest,
        telemetry: Option<&'a RequestTelemetry>,
    ) -> Bm25Runner<'a> {
        Bm25Runner {
            tantivy,
            payload,
            telemetry,
        }
    }

    pub(super) fn run(&self, leaf: Bm25Leaf<'_>) -> anyhow::Result<Vec<loomem_core::SearchResult>> {
        if let Some(meter) = self.telemetry {
            meter.record_bm25_leaf();
        }
        let _timer = stage_timer(self.telemetry.map(|meter| &meter.bm25_us));
        bm25_leaf(self.tantivy, self.payload, leaf)
    }
}

pub(super) fn filter_with_telemetry(
    results: Vec<loomem_core::HybridSearchResult>,
    ctx: &QueryContext,
    payload: &SearchRequest,
    state: &Arc<AppState>,
) -> (FilterOutcome, Option<EvidenceTrace>) {
    let _timer = stage_timer(ctx.telemetry.as_deref().map(|meter| &meter.final_filter_us));
    evidence::filter_with_trace(results, ctx, payload, state)
}

pub(super) fn record_extra_embedding_attempt(ctx: &QueryContext) {
    if let Some(meter) = ctx.telemetry.as_deref() {
        meter.record_extra_embedding_attempt();
    }
}

pub(super) fn build_trace_metadata(
    payload: &SearchRequest,
    state: &AppState,
    start: Instant,
    complexity: QueryComplexity,
    counters: TraceCounters,
) -> Option<TraceMetadata> {
    if !payload.trace && !payload.debug_operation {
        return None;
    }
    Some(TraceMetadata {
        total_results_before_topk: counters.total_results_before_topk,
        dedup_removed: counters.dedup_removed,
        // truncation intentional: preserves the pre-telemetry trace field semantics.
        search_latency_us: start.elapsed().as_micros() as u64,
        query_complexity: state
            .config
            .search
            .complexity
            .enabled
            .then(|| format!("{complexity}")),
        evidence: counters.evidence,
        operation: counters.operation,
    })
}

#[cfg(test)]
mod tests {
    use super::super::stream_scoping_e2e_tests::{
        base_request_for, configured_state, response_projection, seed_one,
    };
    use super::*;

    #[test]
    fn record_saturates_instead_of_wrapping() {
        let slot = AtomicU64::new(u64::MAX);
        RequestTelemetry::record(&slot, Instant::now());
        assert_eq!(slot.load(Ordering::Relaxed), u64::MAX);
    }

    #[tokio::test]
    async fn debug_operation_surfaces_real_bounded_runtime_telemetry() {
        let state = configured_state(true);
        seed_one(&state, "a1", "streamA", "guitar preference enjoy", None).await;
        let mut request = base_request_for("Recommend a guitar");
        request.operation_policy = loomem_core::search::OperationPolicy::E1C;
        request.debug_operation = true;
        let response = response_projection(&state, request).await;
        let operation = &response["trace_metadata"]["operation"];
        assert_eq!(operation["operation_kind"], "PreferenceAdvice");
        assert_eq!(
            operation["operation_plan_sha256"].as_str().unwrap().len(),
            64
        );
        assert_eq!(operation["bm25_leaf_queries"], 2);
        assert!(operation["embedding_calls"].as_u64().unwrap() <= 1);
        assert_eq!(operation["extra_embeddings"], 0);
        assert_eq!(operation["operation_bpe_calls"], 0);
        assert!(operation["temporary_bytes"].as_u64().unwrap() <= 1024 * 1024);
        assert!(!response.to_string().contains("added_terms"));
        assert!(!response.to_string().contains("facets"));
    }

    #[tokio::test]
    async fn debug_e0_and_disabled_e1_report_actual_fallback_identity() {
        let state = configured_state(false);
        seed_one(&state, "a1", "streamA", "guitar preference", None).await;
        let mut e0 = base_request_for("Recommend a guitar");
        e0.debug_operation = true;
        let e0_response = response_projection(&state, e0).await;
        let e0_trace = &e0_response["trace_metadata"]["operation"];
        assert_eq!(e0_trace["operation_kind"], "Plain");
        assert_eq!(e0_trace["fallback_to_e0"], false);
        assert!(e0_trace["bm25_leaf_queries"].as_u64().unwrap() >= 1);

        let mut disabled = base_request_for("Recommend a guitar");
        disabled.operation_policy = loomem_core::search::OperationPolicy::E1C;
        disabled.debug_operation = true;
        let disabled_response = response_projection(&state, disabled).await;
        let disabled_trace = &disabled_response["trace_metadata"]["operation"];
        assert_eq!(disabled_trace["operation_kind"], "Plain");
        assert_eq!(disabled_trace["fallback_to_e0"], true);
        assert!(disabled_trace["bm25_leaf_queries"].as_u64().unwrap() >= 1);
    }
}
