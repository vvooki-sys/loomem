use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use super::{
    bm25_leaf, evidence, operation, Bm25Leaf, FilterOutcome, QueryComplexity, QueryContext,
};
use crate::handlers::types::{EvidenceTrace, OperationTelemetry, SearchRequest, TraceMetadata};
use crate::AppState;

#[cfg(test)]
const E2_MAX_TEMPORARY_BYTES: usize = 1024 * 1024;

pub(super) struct ResponseMetrics {
    pub(super) start: Instant,
    pub(super) complexity: QueryComplexity,
    pub(super) dedup_removed: usize,
    pub(super) total_results_before_topk: usize,
    pub(super) final_top_k: usize,
    pub(super) evidence: Option<EvidenceTrace>,
    pub(super) operation: Option<OperationTelemetry>,
    pub(super) retrieval_v2: Option<E2RetrievalObservation>,
}

pub(super) struct TraceCounters {
    pub(super) total_results_before_topk: usize,
    pub(super) dedup_removed: usize,
    pub(super) evidence: Option<EvidenceTrace>,
    pub(super) operation: Option<OperationTelemetry>,
    pub(super) retrieval_v2: Option<E2RetrievalObservation>,
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
            retrieval_v2: None,
        }
    }

    pub(super) fn tier1(ctx: &QueryContext, outcome: &FilterOutcome) -> Self {
        let metrics = Self::full(ctx, outcome, None, operation::OperationMetrics::default());
        if ctx.operation.policy.is_e2() {
            metrics.with_retrieval_v2(
                super::operation_lane::E2LaneMetrics::primary(&outcome.results, 0)
                    .observation(&ctx.operation, 0),
            )
        } else {
            metrics
        }
    }

    pub(super) fn with_retrieval_v2(mut self, observation: E2RetrievalObservation) -> Self {
        self.retrieval_v2 = Some(observation);
        self
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum E2OperationKind {
    Plain,
    PreferenceAdvice,
    TemporalCompare,
    TemporalEnumerate,
    TemporalWindow,
}

impl E2OperationKind {
    pub(super) fn from_plan(plan: &loomem_core::search::operation::OperationPlan) -> Self {
        use loomem_core::search::operation::QueryOperation;
        match &plan.operation {
            QueryOperation::Plain => Self::Plain,
            QueryOperation::PreferenceAdvice { .. } => Self::PreferenceAdvice,
            QueryOperation::TemporalCompare { .. } => Self::TemporalCompare,
            QueryOperation::TemporalEnumerate { .. } => Self::TemporalEnumerate,
            QueryOperation::TemporalWindow { .. }
            | QueryOperation::TemporalCalendarWindow { .. } => Self::TemporalWindow,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Plain => "Plain",
            Self::PreferenceAdvice => "PreferenceAdvice",
            Self::TemporalCompare => "TemporalCompare",
            Self::TemporalEnumerate => "TemporalEnumerate",
            Self::TemporalWindow => "TemporalWindow",
        }
    }

    fn structural_plan_sha256(self) -> &'static str {
        match self {
            Self::Plain => "4fb0b3e206babc36b5a1469b03cf5e92eef8bffa2e952cd32fd1000986f0d631",
            Self::PreferenceAdvice => {
                "8b161493e53f31a6b4c4fd7eb2b6209e115f47692bb7ee68974a5fb5000f9ef1"
            }
            Self::TemporalCompare => {
                "5154ed433f0d10e7d0f815e1c0dbc2158ad481a6730cfaed1ab4e01fc81cc697"
            }
            Self::TemporalEnumerate => {
                "c440adcfb43987514b407bc3d1214f4ff8be2509939fa01c509b377e0c85a5ce"
            }
            Self::TemporalWindow => {
                "5a6964dc8cf3cc0b17d4b24a11c63e0f826d1b2ae83c7924179059eed0b9caa4"
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum E2LaneStatus {
    NotApplicable,
    Completed,
    Failed,
}

impl E2LaneStatus {
    fn name(self) -> &'static str {
        match self {
            Self::NotApplicable => "not_applicable",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum E2FallbackStage {
    None,
    DatePlan,
    DateLane,
    #[expect(dead_code, reason = "reserved member of the frozen telemetry contract")]
    Merge,
    ResourceLimit,
    Validation,
}

impl E2FallbackStage {
    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::DatePlan => "date_plan",
            Self::DateLane => "date_lane",
            Self::Merge => "merge",
            Self::ResourceLimit => "resource_limit",
            Self::Validation => "validation",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum E2FallbackReason {
    None,
    NoReferenceTime,
    UnsupportedDateExpression,
    DateLeafError,
    ResourceLimit,
    #[expect(dead_code, reason = "reserved member of the frozen telemetry contract")]
    MergeError,
    InvariantViolation,
}

impl E2FallbackReason {
    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::NoReferenceTime => "no_reference_time",
            Self::UnsupportedDateExpression => "unsupported_date_expression",
            Self::DateLeafError => "date_leaf_error",
            Self::ResourceLimit => "resource_limit",
            Self::MergeError => "merge_error",
            Self::InvariantViolation => "invariant_violation",
        }
    }
}

#[derive(Debug)]
pub(super) struct E2LaneObservation {
    pub(super) status: E2LaneStatus,
    pub(super) elapsed_us: u64,
    pub(super) bm25_leaf_attempts: usize,
    pub(super) candidate_count: usize,
    pub(super) candidate_bytes: usize,
}

#[derive(Debug)]
pub(super) struct E2MergeObservation {
    pub(super) elapsed_us: u64,
    pub(super) input_candidates: usize,
    pub(super) unique_candidates: usize,
    pub(super) duplicate_candidates: usize,
    pub(super) output_candidates: usize,
    pub(super) actual_admissions: usize,
    pub(super) actual_removals: usize,
    pub(super) changed_prefix_positions: usize,
    pub(super) protected_prefix_preserved: bool,
}

#[derive(Debug)]
pub(super) struct E2ResourceObservation {
    pub(super) scanned_candidates: usize,
    pub(super) scanned_bytes: usize,
    pub(super) scan_temporary_bytes_upper_bound: usize,
    pub(super) merge_temporary_bytes_upper_bound: usize,
}

#[derive(Debug)]
pub(super) struct E2FallbackObservation {
    pub(super) used: bool,
    pub(super) stage: E2FallbackStage,
    pub(super) reason: E2FallbackReason,
}

#[derive(Debug)]
pub(super) struct E2RetrievalObservation {
    pub(super) operation_kind: E2OperationKind,
    pub(super) primary: E2LaneObservation,
    pub(super) date: E2LaneObservation,
    pub(super) merge: E2MergeObservation,
    pub(super) resources: E2ResourceObservation,
    pub(super) fallback: E2FallbackObservation,
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
    primary_bm25_leaf_queries: AtomicUsize,
    date_bm25_leaf_queries: AtomicUsize,
    embedding_calls: AtomicUsize,
    extra_embedding_calls: AtomicUsize,
    operation_bpe_calls: AtomicUsize,
}

impl RequestTelemetry {
    fn record(slot: &AtomicU64, started: Instant) {
        let elapsed = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        // Saturating accumulate via a CAS loop: `fetch_update` is deprecated on
        // current stable and its replacement `try_update` is above the MSRV.
        let mut current = slot.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_add(elapsed);
            match slot.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    pub(super) fn record_embedding_attempt(&self) {
        self.embedding_calls.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_extra_embedding_attempt(&self) {
        self.record_embedding_attempt();
        self.extra_embedding_calls.fetch_add(1, Ordering::Relaxed);
    }

    fn record_bm25_leaf(&self, lane: Bm25Lane) {
        self.bm25_leaf_queries.fetch_add(1, Ordering::Relaxed);
        let counter = match lane {
            Bm25Lane::Primary => &self.primary_bm25_leaf_queries,
            Bm25Lane::Date => &self.date_bm25_leaf_queries,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn e2_lane_attempts(&self) -> (usize, usize) {
        (
            self.primary_bm25_leaf_queries.load(Ordering::Relaxed),
            self.date_bm25_leaf_queries.load(Ordering::Relaxed),
        )
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
            operation_plan_sha256: (!prepared.policy.is_e2())
                .then(|| prepared.plan.identity_sha256()),
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
    (payload.debug_operation || payload.operation_policy.is_e2())
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
    lane: Bm25Lane,
}

#[derive(Clone, Copy)]
enum Bm25Lane {
    Primary,
    Date,
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
            lane: Bm25Lane::Primary,
        }
    }

    pub(super) fn date_lane<'a>(
        tantivy: &'a loomem_core::TantivyIndex,
        payload: &'a SearchRequest,
        telemetry: Option<&'a RequestTelemetry>,
    ) -> Bm25Runner<'a> {
        Bm25Runner {
            tantivy,
            payload,
            telemetry,
            lane: Bm25Lane::Date,
        }
    }

    pub(super) fn run(&self, leaf: Bm25Leaf<'_>) -> anyhow::Result<Vec<loomem_core::SearchResult>> {
        if let Some(meter) = self.telemetry {
            meter.record_bm25_leaf(self.lane);
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
    let trace = TraceMetadata::new(
        counters.total_results_before_topk,
        counters.dedup_removed,
        // truncation intentional: preserves the pre-telemetry trace field semantics.
        start.elapsed().as_micros() as u64,
        state
            .config
            .search
            .complexity
            .enabled
            .then(|| format!("{complexity}")),
        counters.evidence,
        counters.operation,
    );
    Some(match (payload.debug_operation, counters.retrieval_v2) {
        (true, Some(observation)) => with_retrieval_v2(trace, observation),
        (true, None) => trace,
        (false, _) => trace,
    })
}

fn with_retrieval_v2(trace: TraceMetadata, observation: E2RetrievalObservation) -> TraceMetadata {
    let kind = observation.operation_kind;
    let primary = observation.primary;
    let date = observation.date;
    let merge = observation.merge;
    let resources = observation.resources;
    let fallback = observation.fallback;
    trace.with_retrieval_v2(
        (kind.name(), kind.structural_plan_sha256()),
        (
            primary.status.name(),
            micros_to_ms(primary.elapsed_us),
            primary.bm25_leaf_attempts,
            primary.candidate_count,
            primary.candidate_bytes,
        ),
        (
            date.status.name(),
            micros_to_ms(date.elapsed_us),
            date.bm25_leaf_attempts,
            date.candidate_count,
            date.candidate_bytes,
        ),
        (
            micros_to_ms(merge.elapsed_us),
            merge.input_candidates,
            merge.unique_candidates,
            merge.duplicate_candidates,
            merge.output_candidates,
            merge.actual_admissions,
            merge.actual_removals,
            merge.changed_prefix_positions,
            merge.protected_prefix_preserved,
        ),
        (
            resources.scanned_candidates,
            resources.scanned_bytes,
            resources.scan_temporary_bytes_upper_bound,
            resources.merge_temporary_bytes_upper_bound,
            resources
                .scan_temporary_bytes_upper_bound
                .saturating_add(resources.merge_temporary_bytes_upper_bound),
        ),
        (fallback.used, fallback.stage.name(), fallback.reason.name()),
    )
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

    #[test]
    fn e2_structural_digest_is_value_independent_and_frozen() {
        use loomem_core::search::operation::{
            MAX_CANDIDATES, MAX_CHANGED_ADMISSIONS, MAX_RECORD_SCAN_BYTES, MAX_REQUEST_SCAN_BYTES,
            MAX_RETURNED, PROTECTED_RANKS,
        };

        assert_eq!(MAX_RETURNED, 60);
        assert_eq!(MAX_RETURNED * MAX_RECORD_SCAN_BYTES, 983_040);
        assert_eq!(MAX_RETURNED * 2, 120);
        assert_eq!(PROTECTED_RANKS, 20);
        assert_eq!(MAX_CHANGED_ADMISSIONS, 2);
        assert_eq!(MAX_CANDIDATES, 180);
        assert_eq!(MAX_REQUEST_SCAN_BYTES, 524_288);
        assert_eq!(E2_MAX_TEMPORARY_BYTES, 1_048_576);
        assert_eq!(
            E2OperationKind::TemporalWindow.structural_plan_sha256(),
            "5a6964dc8cf3cc0b17d4b24a11c63e0f826d1b2ae83c7924179059eed0b9caa4"
        );
        assert_eq!(E2OperationKind::TemporalWindow.name(), "TemporalWindow");
    }

    #[test]
    fn e2_allocates_internal_meter_without_debug_response_opt_in() {
        let mut request = base_request_for("last month");
        request.operation_policy = loomem_core::search::OperationPolicy::E2D;
        request.debug_operation = false;
        assert!(request_telemetry(&request).is_some());
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

pub(super) fn temporary_bytes(
    metrics: &operation::OperationMetrics,
    plan: &loomem_core::search::operation::OperationPlan,
) -> usize {
    let scan_buffer = metrics.max_record_bytes.saturating_mul(8);
    let credit_slots = allocation_slots(metrics.matched_candidates);
    let credits = credit_slots.saturating_mul(
        std::mem::size_of::<super::operation_lane::CandidateCredit>()
            .saturating_add(loomem_core::search::operation::MAX_ADDED_BYTES)
            .saturating_add(8usize.saturating_mul(std::mem::size_of::<String>())),
    );
    let reorder = allocation_slots(metrics.scanned_candidates).saturating_mul(
        std::mem::size_of::<Option<loomem_core::HybridSearchResult>>()
            .saturating_add(std::mem::size_of::<loomem_core::HybridSearchResult>()),
    );
    let term_map = 8usize.saturating_mul(
        std::mem::size_of::<(String, usize)>()
            .saturating_add(loomem_core::search::operation::MAX_ADDED_BYTES)
            .saturating_add(128),
    );
    let promoted = loomem_core::search::operation::MAX_CHANGED_ADMISSIONS
        .saturating_mul(std::mem::size_of::<loomem_core::HybridSearchResult>());
    base_temporary_bytes(plan)
        .saturating_add(scan_buffer)
        .saturating_add(credits)
        .saturating_add(reorder)
        .saturating_add(term_map)
        .saturating_add(promoted)
        .saturating_add(loomem_core::search::operation::MAX_ADDED_BYTES.saturating_mul(4))
}

pub(super) fn base_temporary_bytes(plan: &loomem_core::search::operation::OperationPlan) -> usize {
    // The plan identity String and its 64-byte hexadecimal SHA-256 digest are
    // materialized when debug telemetry is finalized. Four bytes per identity
    // byte conservatively covers UTF-8 growth and allocator capacity.
    plan.identity().len().saturating_mul(4).saturating_add(64)
}

fn allocation_slots(items: usize) -> usize {
    items.max(1).next_power_of_two()
}

#[cfg(test)]
mod e2_invariants {
    use super::*;
    use crate::handlers::search::stream_scoping_e2e_tests::configured_state;
    use chrono::{DateTime, Utc};
    use loomem_core::search::operation::{plan, OperationPolicy};
    use serde_json::json;

    fn request(extra: serde_json::Value) -> SearchRequest {
        let mut value = json!({"query":"what happened last month", "operation_policy":"E2-C", "reference_time":"2023-05-30T15:40:00Z"});
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(value).unwrap()
    }

    fn prepared(payload: &SearchRequest) -> operation::PreparedOperation {
        let mut config = configured_state(true).config.search.clone();
        config.multi_query_enabled = false;
        config.rare_term_lane.enabled = false;
        config.rerank_enabled = false;
        operation::prepare(&config, payload, 60, None).unwrap()
    }

    #[test]
    fn calendar_window_is_explicit_and_half_open() {
        let anchor = DateTime::parse_from_rfc3339("2023-05-30T15:40:00Z")
            .unwrap()
            .with_timezone(&Utc);
        for policy in [OperationPolicy::E2D, OperationPolicy::E2C] {
            let value = plan("what happened last month", Some(anchor), policy);
            let interval = value.half_open_interval().unwrap();
            assert_eq!((interval.start, interval.end), (1680307200, 1682899200));
            assert_eq!(interval.inclusive_bounds(), Some((1680307200, 1682899199)));
            for query in [
                "last month last month",
                "last month yesterday",
                "last monthly",
                "yesterday",
            ] {
                assert!(plan(query, Some(anchor), policy)
                    .half_open_interval()
                    .is_none());
            }
            assert!(plan("last month", None, policy)
                .half_open_interval()
                .is_none());
        }
        assert!(plan("last month", Some(anchor), OperationPolicy::E2F)
            .half_open_interval()
            .is_none());
    }

    #[test]
    fn primary_is_e0_and_cache_binds_normalized_anchor() {
        let e2 = request(json!({}));
        let operation = prepared(&e2);
        let date = operation::resolve_date(&e2, &operation);
        let e0 = request(json!({"operation_policy":"E0"}));
        let baseline = prepared(&e0);
        let baseline_date = operation::resolve_date(&e0, &baseline);
        assert_eq!(date.query, baseline_date.query);
        assert_eq!(
            format!("{:?}", date.filter),
            format!("{:?}", baseline_date.filter)
        );
        assert_eq!(operation.primary_expansion("unchanged"), "unchanged");
        let offset = request(json!({"reference_time":"2023-05-30T17:40:00+02:00"}));
        let changed = request(json!({"reference_time":"2023-05-31T15:40:00Z"}));
        assert_eq!(
            operation.cache_identity(&date),
            prepared(&offset).cache_identity(&date)
        );
        assert_ne!(
            operation.cache_identity(&date),
            prepared(&changed).cache_identity(&date)
        );
    }

    #[test]
    fn explicit_dates_validate_before_execution_and_constrain_facet_lane() {
        for date in ["2023-02-30", "2023-2-01", "bad", "10000-01-01"] {
            let payload = request(json!({"date_from":date}));
            assert!(operation::prepare(
                &configured_state(true).config.search.clone(),
                &payload,
                60,
                None
            )
            .is_err());
        }
        let reversed = request(json!({"date_from":"2023-06-01", "date_to":"2023-04-01"}));
        assert!(operation::prepare(
            &configured_state(true).config.search.clone(),
            &reversed,
            60,
            None
        )
        .is_err());
        let facet = request(
            json!({"operation_policy":"E2-F", "query":"recommend a guitar", "date_from":"2023-04-01", "date_to":"2023-04-30"}),
        );
        assert_eq!(
            prepared(&facet).e2_lane_spec().unwrap().inclusive_range,
            Some((1680307200, 1682899199))
        );
        let invalid_anchor = request(json!({"reference_time":"bad"}));
        assert!(prepared(&invalid_anchor).e2_lane_spec().is_none());
    }
}

pub(super) fn charged_bytes(lengths: impl Iterator<Item = usize>) -> usize {
    lengths.fold(0usize, usize::saturating_add)
}
