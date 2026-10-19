//! Bounded, deterministic operation-stage reranking.
use chrono::{DateTime, NaiveDate, Utc};
use loomem_core::hybrid_search::SearchConfig;
use loomem_core::search::operation::{
    OperationPlan, OperationPolicy, MAX_CANDIDATES, MAX_CHANGED_ADMISSIONS, MAX_QUERY_BYTES,
    MAX_RECORD_SCAN_BYTES, MAX_REQUEST_SCAN_BYTES, MAX_RETURNED, PROTECTED_RANKS,
};
use loomem_core::HybridSearchResult;
use std::collections::HashMap;
use std::time::Instant;

use crate::handlers::date_filter::{extract_date_filter, extract_date_filter_at};
use crate::handlers::types::{DateFilter, SearchRequest};

#[derive(Debug, Clone)]
pub(super) struct PreparedOperation {
    pub policy: OperationPolicy,
    pub plan: OperationPlan,
    reference_time: Option<DateTime<Utc>>,
    fallback: Option<&'static str>,
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
        }
    }

    pub fn cache_identity(&self, date: &PreparedDate) -> Option<String> {
        let plan_active = !self.plan.added_terms.is_empty();
        if self.policy == OperationPolicy::E0 || (!plan_active && !date.anchored) {
            return None;
        }
        Some(format!("{:?}|{}", self.policy, self.plan.identity()))
    }
}

pub(super) fn prepare(
    config: &SearchConfig,
    payload: &SearchRequest,
    limit: usize,
    streams: Option<&[String]>,
) -> PreparedOperation {
    let Some(fallback) = gate(config, payload, limit, streams) else {
        let reference_time = normalized_reference_time(payload);
        return PreparedOperation {
            policy: payload.operation_policy,
            plan: loomem_core::search::plan_operation(
                &payload.query,
                reference_time,
                payload.operation_policy,
            ),
            reference_time,
            fallback: None,
        };
    };
    PreparedOperation::baseline(fallback)
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
    if !payload.operation_policy.date_enabled() {
        return None;
    }
    payload
        .reference_time
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

pub(super) fn resolve_date(payload: &SearchRequest, operation: &PreparedOperation) -> PreparedDate {
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
            Utc::now().timestamp(),
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
    pub fallback: Option<&'static str>,
}

pub(super) fn apply(
    candidates: Vec<HybridSearchResult>,
    prepared: &PreparedOperation,
    debug: bool,
) -> Vec<HybridSearchResult> {
    let (ranked, mut metrics) = if prepared.policy.facets_enabled() {
        rerank(candidates, &prepared.plan)
    } else {
        (candidates, OperationMetrics::default())
    };
    if metrics.fallback.is_none() {
        metrics.fallback = prepared.fallback;
    }
    trace(prepared.policy, &metrics, debug);
    ranked
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
        elapsed_us = metrics.elapsed_us,
        fallback = metrics.fallback,
        "operation search stage"
    );
}

struct CandidateCredit {
    index: usize,
    utility: f64,
    terms: Vec<String>,
}

pub(super) fn rerank(
    candidates: Vec<HybridSearchResult>,
    plan: &OperationPlan,
) -> (Vec<HybridSearchResult>, OperationMetrics) {
    let started = Instant::now();
    let mut metrics = OperationMetrics {
        planned_terms: plan.added_terms.len(),
        ..Default::default()
    };
    if plan.added_terms.is_empty() {
        metrics.elapsed_us = started.elapsed().as_micros();
        return (candidates, metrics);
    }
    if candidates.len() > MAX_CANDIDATES {
        metrics.fallback = Some("candidate_limit");
        metrics.elapsed_us = started.elapsed().as_micros();
        return (candidates, metrics);
    }
    let Some(credits) = scan(&candidates, plan, &mut metrics) else {
        metrics.fallback = Some("scan_limit");
        metrics.elapsed_us = started.elapsed().as_micros();
        return (candidates, metrics);
    };
    let promoted = choose(credits);
    metrics.changed_admissions = promoted.len();
    metrics.elapsed_us = started.elapsed().as_micros();
    (move_after_anchors(candidates, &promoted), metrics)
}

fn scan(
    candidates: &[HybridSearchResult],
    plan: &OperationPlan,
    metrics: &mut OperationMetrics,
) -> Option<Vec<CandidateCredit>> {
    let mut credits = Vec::new();
    for (index, candidate) in candidates.iter().enumerate() {
        let scan_bytes = candidate.content.len().min(MAX_RECORD_SCAN_BYTES);
        metrics.scanned_bytes = metrics.scanned_bytes.checked_add(scan_bytes)?;
        if metrics.scanned_bytes > MAX_REQUEST_SCAN_BYTES {
            return None;
        }
        metrics.scanned_candidates += 1;
        if index < PROTECTED_RANKS {
            continue;
        }
        let prefix = String::from_utf8_lossy(&candidate.content.as_bytes()[..scan_bytes]);
        let lower = prefix.to_lowercase();
        let terms: Vec<String> = plan
            .added_terms
            .iter()
            .filter(|term| {
                lower
                    .split(|c: char| !c.is_alphanumeric())
                    .any(|word| word == term.as_str())
            })
            .cloned()
            .collect();
        if terms.is_empty() {
            continue;
        }
        metrics.matched_candidates += 1;
        let rank_prior = 1.0 / f64::from(u32::try_from(index + 1).ok()?);
        let novelty = f64::from(u32::try_from(terms.len()).ok()?)
            / f64::from(u32::try_from(plan.added_terms.len()).ok()?);
        credits.push(CandidateCredit {
            index,
            utility: rank_prior + novelty.min(rank_prior),
            terms,
        });
    }
    Some(credits)
}

fn choose(mut credits: Vec<CandidateCredit>) -> Vec<usize> {
    credits.sort_by(|left, right| {
        right
            .utility
            .total_cmp(&left.utility)
            .then_with(|| left.index.cmp(&right.index))
    });
    let mut term_uses: HashMap<String, usize> = HashMap::new();
    let mut selected = Vec::new();
    for credit in credits {
        let earns_credit = credit
            .terms
            .iter()
            .any(|term| term_uses.get(term).copied().unwrap_or(0) < MAX_CHANGED_ADMISSIONS);
        if !earns_credit {
            continue;
        }
        for term in credit.terms {
            let count = term_uses.entry(term).or_default();
            *count = (*count + 1).min(MAX_CHANGED_ADMISSIONS);
        }
        selected.push(credit.index);
        if selected.len() == MAX_CHANGED_ADMISSIONS {
            break;
        }
    }
    selected.sort_unstable();
    selected
}

fn move_after_anchors(
    candidates: Vec<HybridSearchResult>,
    promoted: &[usize],
) -> Vec<HybridSearchResult> {
    if promoted.is_empty() {
        return candidates;
    }
    let mut slots: Vec<Option<HybridSearchResult>> = candidates.into_iter().map(Some).collect();
    let promoted_records: Vec<_> = promoted
        .iter()
        .filter_map(|index| slots.get_mut(*index).and_then(Option::take))
        .collect();
    let mut output = Vec::with_capacity(slots.len());
    output.extend(
        slots
            .iter_mut()
            .take(PROTECTED_RANKS)
            .filter_map(Option::take),
    );
    output.extend(promoted_records);
    output.extend(slots.into_iter().skip(PROTECTED_RANKS).flatten());
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use loomem_core::search::{OperationPlan, QueryOperation};

    fn candidate(index: usize, content: &str) -> HybridSearchResult {
        HybridSearchResult {
            id: format!("r{index:03}"),
            content: content.to_string(),
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

    fn plan() -> OperationPlan {
        OperationPlan {
            operation: QueryOperation::PreferenceAdvice {
                topic: "guitar".into(),
            },
            facets: vec!["guitar".into()],
            added_terms: vec!["prefer".into(), "enjoy".into()],
        }
    }

    #[test]
    fn protects_top_twenty_and_changes_at_most_two() {
        let input: Vec<_> = (0..80)
            .map(|index| candidate(index, if index >= 35 { "prefer enjoy" } else { "plain" }))
            .collect();
        let baseline: Vec<_> = input.iter().map(|item| item.id.clone()).collect();
        let (ranked, metrics) = rerank(input, &plan());
        assert_eq!(
            ranked[..20].iter().map(|item| &item.id).collect::<Vec<_>>(),
            baseline[..20].iter().collect::<Vec<_>>()
        );
        assert_eq!(metrics.changed_admissions, 2);
        assert_eq!(&ranked[20].id, "r035");
        assert_eq!(&ranked[21].id, "r036");
    }

    #[test]
    fn repetition_does_not_change_priority() {
        let mut once: Vec<_> = (0..60).map(|index| candidate(index, "plain")).collect();
        once[40].content = "prefer enjoy".into();
        once[41].content = "prefer enjoy".repeat(100);
        let (ranked, _) = rerank(once, &plan());
        assert_eq!(&ranked[20].id, "r040");
        assert_eq!(&ranked[21].id, "r041");
    }

    #[test]
    fn copied_query_flood_is_saturated_at_two() {
        let input: Vec<_> = (0..60)
            .map(|index| candidate(index, if index >= 35 { "prefer" } else { "plain" }))
            .collect();
        let (_, metrics) = rerank(input, &plan());
        assert_eq!(metrics.changed_admissions, 2);
        assert_eq!(metrics.matched_candidates, 25);
    }

    #[test]
    fn request_scan_overflow_fails_open_byte_identical() {
        let input: Vec<_> = (0..40)
            .map(|index| candidate(index, &"x".repeat(MAX_RECORD_SCAN_BYTES)))
            .collect();
        let ids: Vec<_> = input.iter().map(|item| item.id.clone()).collect();
        let (ranked, metrics) = rerank(input, &plan());
        assert_eq!(
            ranked
                .iter()
                .map(|item| item.id.clone())
                .collect::<Vec<_>>(),
            ids
        );
        assert_eq!(metrics.fallback, Some("scan_limit"));
    }

    #[test]
    fn candidate_limit_fails_open_byte_identical() {
        let input: Vec<_> = (0..=MAX_CANDIDATES)
            .map(|index| candidate(index, "prefer"))
            .collect();
        let ids: Vec<_> = input.iter().map(|item| item.id.clone()).collect();
        let (ranked, metrics) = rerank(input, &plan());
        assert_eq!(
            ranked
                .iter()
                .map(|item| item.id.clone())
                .collect::<Vec<_>>(),
            ids
        );
        assert_eq!(metrics.fallback, Some("candidate_limit"));
    }

    fn valid_gate() -> GateState {
        GateState {
            operation_enabled: true,
            configured_top_k: 10,
            multi_query: false,
            rare_lane: false,
            multi_stream: false,
            downstream_rerank: false,
            requested_top_k: 60,
            query_bytes: 32,
        }
    }

    #[test]
    fn request_and_candidate_budgets_are_fail_closed() {
        assert_eq!(gate_reason(valid_gate()), None);
        let mut too_many_returned = valid_gate();
        too_many_returned.requested_top_k = 61;
        assert_eq!(gate_reason(too_many_returned), Some("returned_limit"));
        let mut too_many_candidates = valid_gate();
        too_many_candidates.configured_top_k = (MAX_CANDIDATES / 3) + 1;
        assert_eq!(gate_reason(too_many_candidates), Some("returned_limit"));
    }

    #[test]
    fn extra_retrieval_leaves_disable_operation_path() {
        let mutators: [fn(&mut GateState); 3] = [
            |state: &mut GateState| state.multi_query = true,
            |state: &mut GateState| state.rare_lane = true,
            |state: &mut GateState| state.multi_stream = true,
        ];
        for mutate in mutators {
            let mut state = valid_gate();
            mutate(&mut state);
            assert!(gate_reason(state).is_some());
        }
    }

    #[test]
    fn oversized_query_disables_date_and_combined_cells() {
        for policy in [OperationPolicy::E1D, OperationPolicy::E1C] {
            let mut state = valid_gate();
            state.query_bytes = MAX_QUERY_BYTES + 1;
            assert_eq!(gate_reason(state), Some("query_limit"), "{policy:?}");
        }
    }

    #[test]
    fn cache_identity_uses_effective_dates_instead_of_anchor_text() {
        let make = |hour| PreparedOperation {
            policy: OperationPolicy::E1D,
            plan: OperationPlan::plain(),
            reference_time: Utc.with_ymd_and_hms(2026, 10, 19, hour, 0, 0).single(),
            fallback: None,
        };
        let date = PreparedDate {
            query: "yesterday".into(),
            filter: Some(DateFilter::Range(1, 2)),
            anchored: true,
        };
        assert_eq!(
            make(1).cache_identity(&date),
            make(23).cache_identity(&date)
        );
    }

    #[test]
    fn rerank_is_deterministic_and_nonmutating() {
        let input: Vec<_> = (0..60)
            .map(|index| candidate(index, if index == 45 { "prefer" } else { "plain" }))
            .collect();
        let first = rerank(input.clone(), &plan()).0;
        let second = rerank(input.clone(), &plan()).0;
        assert_eq!(
            first.iter().map(|item| &item.id).collect::<Vec<_>>(),
            second.iter().map(|item| &item.id).collect::<Vec<_>>()
        );
        let originals: HashMap<_, _> = input.iter().map(|item| (&item.id, &item.content)).collect();
        assert!(first
            .iter()
            .all(|item| originals.get(&item.id) == Some(&&item.content)));
    }
}
