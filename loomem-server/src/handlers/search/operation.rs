//! Bounded, deterministic operation-stage reranking.
pub(super) use super::operation_lane::rerank;
use super::telemetry::{base_temporary_bytes, temporary_bytes};
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use loomem_core::hybrid_search::SearchConfig;
use loomem_core::search::operation::{
    OperationPlan, OperationPolicy, MAX_QUERY_BYTES, MAX_RETURNED,
};
#[cfg(test)]
use loomem_core::search::operation::{
    MAX_CANDIDATES, MAX_RECORD_SCAN_BYTES, MAX_REQUEST_SCAN_BYTES,
};
use loomem_core::HybridSearchResult;
use std::collections::HashMap;

use crate::handlers::date_filter::{extract_date_filter, extract_date_filter_at};
use crate::handlers::types::{DateFilter, SearchRequest};
use crate::handlers::AppError;

const DATE_SOURCE_POLICY: &str = "event-date-only-v1";
const E2_CACHE_VERSION: &str = "e3-two-lane-v1";
const E5_CACHE_VERSION: &str = "e5-context-relevance-v1";
const E4_CACHE_VERSION: &str = "e4-context-selection-v1";

#[derive(Debug, Clone)]
pub(super) struct E2LaneSpec {
    pub query: String,
    pub inclusive_range: Option<(i64, i64)>,
}

#[derive(Debug, Clone)]
pub(super) struct PreparedOperation {
    pub policy: OperationPolicy,
    pub plan: OperationPlan,
    pub(super) reference_time: Option<DateTime<Utc>>,
    fallback: Option<&'static str>,
    e2_lane: Option<E2LaneSpec>,
    e2_fallback: Option<&'static str>,
    explicit_range: Option<DateFilter>,
}

pub(super) struct PreparedDate {
    pub query: String,
    pub filter: Option<DateFilter>,
    anchored: bool,
}

impl PreparedOperation {
    fn baseline(fallback: Option<&'static str>) -> Self {
        Self {
            policy: OperationPolicy::E0,
            plan: OperationPlan::plain(),
            reference_time: None,
            fallback,
            e2_lane: None,
            e2_fallback: None,
            explicit_range: None,
        }
    }

    pub fn cache_identity(&self, date: &PreparedDate) -> Option<String> {
        if self.policy == OperationPolicy::E6 {
            return Some(format!(
                "e6-semantic-evidence-v2-temporal-veto-v1|{:?}",
                self.reference_time
            ));
        }
        if self.policy == OperationPolicy::E5 {
            return Some(E5_CACHE_VERSION.to_string());
        }
        if self.policy == OperationPolicy::E4 {
            return Some(E4_CACHE_VERSION.to_string());
        }
        if self.policy.is_e2() {
            return Some(self.e2_cache_identity());
        }
        let plan_active = !self.plan.added_terms.is_empty();
        if self.policy == OperationPolicy::E0 || (!plan_active && !date.anchored) {
            return None;
        }
        Some(format!("{:?}|{}", self.policy, self.plan.identity()))
    }

    pub fn fallback_reason(&self) -> Option<&'static str> {
        self.fallback
    }

    #[cfg(feature = "evaluation")]
    pub(super) fn evidence_reference_time(&self) -> Option<DateTime<Utc>> {
        self.reference_time
    }

    pub(super) fn e2_lane_spec(&self) -> Option<&E2LaneSpec> {
        self.e2_lane.as_ref()
    }

    pub(super) fn e2_fallback_reason(&self) -> Option<&'static str> {
        self.e2_fallback
    }

    pub(super) fn primary_expansion(&self, expanded: &str) -> String {
        if self.policy.is_e2() {
            expanded.to_string()
        } else {
            self.plan.expand(expanded)
        }
    }

    pub(super) fn uses_operation_primary_merge(&self) -> bool {
        !self.policy.is_e2() && !self.plan.added_terms.is_empty()
    }

    fn e2_cache_identity(&self) -> String {
        let range = self.e2_lane.as_ref().and_then(|lane| lane.inclusive_range);
        serde_json::json!({
            "version": E2_CACHE_VERSION, "policy": self.policy,
            "plan": self.plan.identity(), "range": range,
            "anchor": self.reference_time, "date_source": DATE_SOURCE_POLICY,
            "explicit_precedence": "intersection", "caps": [60, 180, 20, 2, 524288, 1048576],
        })
        .to_string()
    }
}

pub(super) fn prepare(
    config: &SearchConfig,
    payload: &SearchRequest,
    limit: usize,
    streams: Option<&[String]>,
) -> Result<PreparedOperation, AppError> {
    if payload.operation_policy.is_e2() {
        return prepare_e2(config, payload, limit, streams);
    }
    let Some(fallback) = gate(config, payload, limit, streams) else {
        let reference_time = normalized_reference_time(payload);
        return Ok(PreparedOperation {
            policy: payload.operation_policy,
            plan: loomem_core::search::plan_operation(
                &payload.query,
                reference_time,
                payload.operation_policy,
            ),
            reference_time,
            fallback: None,
            e2_lane: None,
            e2_fallback: None,
            explicit_range: None,
        });
    };
    Ok(PreparedOperation::baseline(fallback))
}

fn prepare_e2(
    config: &SearchConfig,
    payload: &SearchRequest,
    limit: usize,
    streams: Option<&[String]>,
) -> Result<PreparedOperation, AppError> {
    let explicit_range = strict_explicit_date_filter(payload)?;
    let reference_time = normalized_reference_time(payload);
    let plan = loomem_core::search::plan_operation(
        &payload.query,
        reference_time,
        payload.operation_policy,
    );
    let gate_fallback = gate(config, payload, limit, streams).flatten();
    let mut prepared = PreparedOperation {
        policy: payload.operation_policy,
        plan,
        reference_time,
        fallback: gate_fallback,
        e2_lane: None,
        e2_fallback: None,
        explicit_range,
    };
    if gate_fallback.is_some() || unsupported_e2_filters(payload) {
        return Ok(prepared);
    }
    prepare_e2_lane(payload, &mut prepared);
    Ok(prepared)
}

fn prepare_e2_lane(payload: &SearchRequest, prepared: &mut PreparedOperation) {
    let interval = prepared.plan.half_open_interval();
    let requires_date = requires_derived_date(payload, prepared);
    if requires_date && prepared.reference_time.is_none() {
        prepared.e2_fallback = Some("no_reference_time");
        return;
    }
    let range = interval.and_then(|value| value.inclusive_bounds());
    if requires_date && range.is_none() {
        prepared.e2_fallback = Some("unsupported_date_expression");
        return;
    }
    let Ok(range) = effective_lane_range(range, prepared.explicit_range.as_ref()) else {
        return;
    };
    let facet_query = prepared.plan.expand(&payload.query);
    let query = if payload.operation_policy.facets_enabled() {
        facet_query
    } else {
        payload.query.clone()
    };
    if payload.operation_policy.facets_enabled() && prepared.plan.kind() == "Plain" {
        return;
    }
    prepared.e2_lane = Some(E2LaneSpec {
        query,
        inclusive_range: range,
    });
}

fn requires_derived_date(payload: &SearchRequest, prepared: &PreparedOperation) -> bool {
    if !payload.operation_policy.date_enabled() {
        return false;
    }
    if !payload.operation_policy.facets_enabled() || prepared.plan.half_open_interval().is_some() {
        return true;
    }
    loomem_core::search::operation::has_date_window_marker(&payload.query)
}

#[cfg(test)]
#[path = "operation/facet_gate_tests.rs"]
mod facet_gate_tests;

fn effective_lane_range(
    range: Option<(i64, i64)>,
    explicit: Option<&DateFilter>,
) -> Result<Option<(i64, i64)>, ()> {
    match (range, explicit) {
        (Some(derived), Some(DateFilter::Range(start, end))) => {
            super::operation_lane::intersect_ranges(derived, (*start, *end))
                .map(Some)
                .ok_or(())
        }
        (None, Some(DateFilter::Range(start, end))) => Ok(Some((*start, *end))),
        (range, _) => Ok(range),
    }
}

fn gate(
    config: &SearchConfig,
    payload: &SearchRequest,
    limit: usize,
    streams: Option<&[String]>,
) -> Option<Option<&'static str>> {
    if payload.operation_policy == OperationPolicy::E0 {
        return Some(None);
    }
    gate_reason(GateState {
        operation_enabled: config.operation.enabled,
        configured_top_k: config.top_k,
        multi_query: config.multi_query_enabled,
        rare_lane: config.rare_term_lane.enabled,
        multi_stream: streams.is_some_and(|items| items.len() > 1),
        downstream_rerank: config.rerank_enabled,
        requested_top_k: limit,
        query_bytes: payload.query.len(),
    })
    .map(Some)
}

struct GateState {
    operation_enabled: bool,
    configured_top_k: usize,
    multi_query: bool,
    rare_lane: bool,
    multi_stream: bool,
    downstream_rerank: bool,
    requested_top_k: usize,
    query_bytes: usize,
}

fn gate_reason(state: GateState) -> Option<&'static str> {
    let reason = if !state.operation_enabled {
        "disabled"
    } else if state.requested_top_k > MAX_RETURNED || state.configured_top_k > MAX_RETURNED {
        "returned_limit"
    } else if state.query_bytes > MAX_QUERY_BYTES {
        "query_limit"
    } else if state.multi_query {
        "multi_query_limit"
    } else if state.rare_lane {
        "rare_term_lane"
    } else if state.multi_stream {
        "bm25_leaf_limit"
    } else if state.downstream_rerank {
        "downstream_rerank"
    } else {
        return None;
    };
    Some(reason)
}

fn normalized_reference_time(payload: &SearchRequest) -> Option<DateTime<Utc>> {
    if !payload.operation_policy.date_enabled() && payload.operation_policy != OperationPolicy::E6 {
        return None;
    }
    payload
        .reference_time
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
        .filter(|value| !payload.operation_policy.is_e2() || (1970..=9999).contains(&value.year()))
}

pub(super) fn resolve_date(payload: &SearchRequest, operation: &PreparedOperation) -> PreparedDate {
    if operation.policy.is_e2() {
        if payload.date_from.is_some() || payload.date_to.is_some() {
            return PreparedDate {
                query: payload.query.clone(),
                filter: explicit_date_filter(payload),
                anchored: false,
            };
        }
        let (query, filter) = extract_date_filter(&payload.query);
        return PreparedDate {
            query,
            filter,
            anchored: false,
        };
    }
    if payload.date_from.is_some() || payload.date_to.is_some() {
        return PreparedDate {
            query: payload.query.clone(),
            filter: explicit_date_filter(payload),
            anchored: false,
        };
    }
    if operation.policy.date_enabled() {
        return anchored_date(payload, operation.reference_time);
    }
    let (query, filter) = extract_date_filter(&payload.query);
    PreparedDate {
        query,
        filter,
        anchored: false,
    }
}

fn strict_explicit_date_filter(payload: &SearchRequest) -> Result<Option<DateFilter>, AppError> {
    if payload.date_from.is_none() && payload.date_to.is_none() {
        return Ok(None);
    }
    let start = strict_bound(payload.date_from.as_deref(), false)?;
    let end = strict_bound(payload.date_to.as_deref(), true)?;
    if start > end {
        return Err(invalid_explicit_date());
    }
    Ok(Some(DateFilter::Range(start, end)))
}

fn strict_bound(value: Option<&str>, end: bool) -> Result<i64, AppError> {
    let value = value.unwrap_or(if end { "9999-12-31" } else { "1970-01-01" });
    let parsed = if value.len() == 10 {
        let date =
            NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| invalid_explicit_date())?;
        if date.to_string() != value {
            return Err(invalid_explicit_date());
        }
        let time = if end { (23, 59, 59) } else { (0, 0, 0) };
        date.and_hms_opt(time.0, time.1, time.2)
            .map(|value| value.and_utc())
            .ok_or_else(invalid_explicit_date)?
    } else {
        DateTime::parse_from_rfc3339(value)
            .map_err(|_| invalid_explicit_date())?
            .with_timezone(&Utc)
    };
    if !(1970..=9999).contains(&parsed.year()) {
        return Err(invalid_explicit_date());
    }
    Ok(parsed.timestamp())
}

fn invalid_explicit_date() -> AppError {
    AppError::BadRequest("invalid explicit date range".to_string())
}

pub(super) fn merge_expanded_results(
    original: Vec<loomem_core::SearchResult>,
    expanded: Vec<loomem_core::SearchResult>,
) -> Vec<loomem_core::SearchResult> {
    let mut merged: HashMap<_, _> = original
        .into_iter()
        .enumerate()
        .map(|(rank, item)| (item.id.clone(), (item, rank)))
        .collect();
    for mut result in expanded {
        result.score *= 0.5;
        merged
            .entry(result.id.clone())
            .and_modify(|(existing, _)| existing.score = existing.score.max(result.score))
            .or_insert((result, usize::MAX));
    }
    let mut results: Vec<_> = merged.into_values().collect();
    results.sort_by(|(left, left_rank), (right, right_rank)| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left_rank.cmp(right_rank))
            .then_with(|| left.id.cmp(&right.id))
    });
    results.into_iter().map(|(item, _)| item).collect()
}

fn anchored_date(payload: &SearchRequest, anchor: Option<DateTime<Utc>>) -> PreparedDate {
    let Some(anchor) = anchor else {
        return PreparedDate {
            query: payload.query.clone(),
            filter: None,
            anchored: false,
        };
    };
    let (query, filter) = extract_date_filter_at(&payload.query, anchor);
    PreparedDate {
        query,
        anchored: filter.is_some(),
        filter,
    }
}

fn explicit_date_filter(payload: &SearchRequest) -> Option<DateFilter> {
    match (payload.date_from.as_deref(), payload.date_to.as_deref()) {
        (Some(from), Some(to)) => Some(DateFilter::Range(start_of_day(from)?, end_of_day(to)?)),
        (Some(from), None) => Some(DateFilter::Range(
            start_of_day(from)?,
            crate::evaluation_clock::now().timestamp(),
        )),
        (None, Some(to)) => Some(DateFilter::Range(0, end_of_day(to)?)),
        (None, None) => None,
    }
}

fn start_of_day(value: &str) -> Option<i64> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)
        .map(|value| value.and_utc().timestamp())
}

fn end_of_day(value: &str) -> Option<i64> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(23, 59, 59)
        .map(|value| value.and_utc().timestamp())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct OperationMetrics {
    pub planned_terms: usize,
    pub scanned_candidates: usize,
    pub scanned_bytes: usize,
    pub matched_candidates: usize,
    pub changed_admissions: usize,
    pub elapsed_us: u128,
    pub temporary_bytes: usize,
    pub fallback: Option<&'static str>,
    pub(super) max_record_bytes: usize,
}

pub(super) fn apply(
    candidates: Vec<HybridSearchResult>,
    prepared: &PreparedOperation,
    debug: bool,
) -> (Vec<HybridSearchResult>, OperationMetrics) {
    if prepared.policy.is_e2() {
        return (candidates, OperationMetrics::default());
    }
    let (ranked, mut metrics) = if prepared.policy.facets_enabled() {
        rerank(candidates, &prepared.plan)
    } else {
        (candidates, OperationMetrics::default())
    };
    if debug {
        metrics.temporary_bytes = base_temporary_bytes(&prepared.plan);
        if prepared.policy.facets_enabled() {
            metrics.temporary_bytes = temporary_bytes(&metrics, &prepared.plan);
        }
    }
    if metrics.fallback.is_none() {
        metrics.fallback = prepared.fallback;
    }
    trace(prepared.policy, &metrics, debug);
    (ranked, metrics)
}

fn trace(policy: OperationPolicy, metrics: &OperationMetrics, enabled: bool) {
    if !enabled {
        return;
    }
    tracing::info!(
        policy = ?policy,
        planned_terms = metrics.planned_terms,
        scanned_candidates = metrics.scanned_candidates,
        scanned_bytes = metrics.scanned_bytes,
        matched_candidates = metrics.matched_candidates,
        changed_admissions = metrics.changed_admissions,
        temporary_bytes = metrics.temporary_bytes,
        elapsed_us = metrics.elapsed_us,
        fallback = metrics.fallback,
        "operation search stage"
    );
}

#[cfg(test)]
#[path = "operation/tests.rs"]
mod tests;

fn unsupported_e2_filters(payload: &SearchRequest) -> bool {
    payload.entity.is_some()
        || payload.user_id.is_some()
        || payload.filters.is_some()
        || payload.debug_channels
        || payload.debug_signal_breakdown
}

#[cfg(test)]
#[path = "operation/e4_tests.rs"]
mod e4_tests;

#[cfg(test)]
mod e5_tests;

#[cfg(test)]
#[path = "operation/e6_tests.rs"]
mod e6_tests;

#[cfg(all(test, feature = "evaluation"))]
#[path = "operation/e6_cost_tests.rs"]
mod e6_cost_tests;
