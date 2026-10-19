//! Bounded E2 retrieval lane and deterministic post-primary union.
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use chrono::NaiveDate;
use loomem_core::storage::{Chunk, FactType};
use loomem_core::{HybridSearchResult, SearchResult};

use super::operation::{E2LaneSpec, PreparedOperation};
use super::telemetry::{
    E2FallbackObservation, E2LaneObservation, E2LaneStatus, E2MergeObservation, E2OperationKind,
    E2ResourceObservation, E2RetrievalObservation,
};
use super::{Bm25Leaf, Bm25Runner, QueryContext};
use crate::handlers::types::SearchRequest;
use crate::AppState;

const LANE_CANDIDATES_MAX: usize = 60;
const PROTECTED_PREFIX: usize = 20;
const ADMISSIONS_MAX: usize = 2;
const RECORD_SCAN_BYTES_MAX: usize = 16 * 1024;
const REQUEST_SCAN_BYTES_MAX: usize = 512 * 1024;
const TEMPORARY_BYTES_MAX: usize = 1024 * 1024;

use super::operation::OperationMetrics;
use super::telemetry::charged_bytes;
use super::telemetry::{
    E2FallbackReason as FallbackReason, E2FallbackStage as FallbackStage,
    E2LaneStatus as LaneStatus,
};
use loomem_core::search::operation::{
    OperationPlan, MAX_CANDIDATES, MAX_CHANGED_ADMISSIONS, MAX_RECORD_SCAN_BYTES,
    MAX_REQUEST_SCAN_BYTES, PROTECTED_RANKS,
};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub(super) struct E2LaneMetrics {
    pub primary_elapsed_us: u128,
    pub primary_candidate_count: usize,
    pub primary_candidate_bytes: usize,
    pub lane_status: LaneStatus,
    pub lane_elapsed_us: u128,
    pub lane_leaf_attempts: usize,
    pub lane_candidate_count: usize,
    pub lane_candidate_bytes: usize,
    pub merge_elapsed_us: u128,
    pub merge_input_candidates: usize,
    pub merge_unique_candidates: usize,
    pub merge_duplicate_candidates: usize,
    pub merge_output_candidates: usize,
    pub actual_admissions: usize,
    pub actual_removals: usize,
    pub changed_prefix_positions: usize,
    pub protected_prefix_preserved: bool,
    pub scanned_candidates: usize,
    pub scanned_bytes: usize,
    pub scan_temporary_bytes_upper_bound: usize,
    pub merge_temporary_bytes_upper_bound: usize,
    pub temporary_bytes_upper_bound: usize,
    pub fallback_stage: FallbackStage,
    pub fallback_reason: FallbackReason,
}

impl E2LaneMetrics {
    pub(super) fn primary(results: &[HybridSearchResult], elapsed_us: u128) -> Self {
        Self {
            primary_elapsed_us: elapsed_us,
            primary_candidate_count: results.len(),
            primary_candidate_bytes: charged_bytes(results.iter().map(|item| item.content.len())),
            lane_status: LaneStatus::NotApplicable,
            lane_elapsed_us: 0,
            lane_leaf_attempts: 0,
            lane_candidate_count: 0,
            lane_candidate_bytes: 0,
            merge_elapsed_us: 0,
            merge_input_candidates: results.len(),
            merge_unique_candidates: results.len(),
            merge_duplicate_candidates: 0,
            merge_output_candidates: results.len(),
            actual_admissions: 0,
            actual_removals: 0,
            changed_prefix_positions: 0,
            protected_prefix_preserved: true,
            scanned_candidates: 0,
            scanned_bytes: 0,
            scan_temporary_bytes_upper_bound: 0,
            merge_temporary_bytes_upper_bound: 0,
            temporary_bytes_upper_bound: 0,
            fallback_stage: FallbackStage::None,
            fallback_reason: FallbackReason::None,
        }
    }

    fn fail(&mut self, stage: FallbackStage, reason: FallbackReason) {
        self.lane_status = LaneStatus::Failed;
        self.fallback_stage = stage;
        self.fallback_reason = reason;
    }

    pub(super) fn observation(
        self,
        prepared: &PreparedOperation,
        primary_leaf_attempts: usize,
    ) -> E2RetrievalObservation {
        E2RetrievalObservation {
            operation_kind: E2OperationKind::from_plan(&prepared.plan),
            primary: E2LaneObservation {
                status: E2LaneStatus::Completed,
                elapsed_us: bounded_us(self.primary_elapsed_us),
                bm25_leaf_attempts: primary_leaf_attempts,
                candidate_count: self.primary_candidate_count,
                candidate_bytes: self.primary_candidate_bytes,
            },
            date: E2LaneObservation {
                status: self.lane_status,
                elapsed_us: bounded_us(self.lane_elapsed_us),
                bm25_leaf_attempts: self.lane_leaf_attempts,
                candidate_count: self.lane_candidate_count,
                candidate_bytes: self.lane_candidate_bytes,
            },
            merge: E2MergeObservation {
                elapsed_us: bounded_us(self.merge_elapsed_us),
                input_candidates: self.merge_input_candidates,
                unique_candidates: self.merge_unique_candidates,
                duplicate_candidates: self.merge_duplicate_candidates,
                output_candidates: self.merge_output_candidates,
                actual_admissions: self.actual_admissions,
                actual_removals: self.actual_removals,
                changed_prefix_positions: self.changed_prefix_positions,
                protected_prefix_preserved: self.protected_prefix_preserved,
            },
            resources: E2ResourceObservation {
                scanned_candidates: self.scanned_candidates,
                scanned_bytes: self.scanned_bytes,
                scan_temporary_bytes_upper_bound: self.scan_temporary_bytes_upper_bound,
                merge_temporary_bytes_upper_bound: self.merge_temporary_bytes_upper_bound,
            },
            fallback: E2FallbackObservation {
                used: !matches!(self.fallback_reason, FallbackReason::None),
                stage: self.fallback_stage,
                reason: self.fallback_reason,
            },
        }
    }
}

fn bounded_us(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

pub(super) struct LaneOutcome {
    pub results: Vec<HybridSearchResult>,
    observation: Option<E2RetrievalObservation>,
}

impl LaneOutcome {
    pub(super) fn finish(
        self,
        metrics: super::telemetry::ResponseMetrics,
    ) -> (Vec<HybridSearchResult>, super::telemetry::ResponseMetrics) {
        let metrics = match self.observation {
            Some(observation) => metrics.with_retrieval_v2(observation),
            None => metrics,
        };
        (self.results, metrics)
    }
}

struct LaneRequest<'a> {
    ctx: &'a QueryContext,
    payload: &'a SearchRequest,
    state: &'a Arc<AppState>,
    prepared: &'a PreparedOperation,
}

/// Execute one E2 lexical leaf and union eligible results after the protected
/// primary prefix. Every fallible step completes before the primary vector is
/// mutated, so a failure returns it byte-for-byte.
pub(super) async fn finalize(
    primary: Vec<HybridSearchResult>,
    primary_elapsed_us: u128,
    ctx: &QueryContext,
    payload: &SearchRequest,
    state: &Arc<AppState>,
) -> LaneOutcome {
    if !ctx.operation.policy.is_e2() {
        return LaneOutcome {
            results: primary,
            observation: None,
        };
    }
    let mut metrics = E2LaneMetrics::primary(&primary, primary_elapsed_us);
    let request = LaneRequest {
        ctx,
        payload,
        state,
        prepared: &ctx.operation,
    };
    let Some(spec) = request.prepared.e2_lane_spec() else {
        apply_preparation_fallback(&mut metrics, request.prepared);
        return observed(
            primary,
            metrics,
            request.prepared,
            primary_attempts(request.ctx),
        );
    };
    let attempts = primary_attempts(request.ctx);
    if primary.len() <= PROTECTED_PREFIX || attempts >= 2 {
        return observed(primary, metrics, request.prepared, attempts);
    }
    let started = Instant::now();
    let attempts_before = request
        .ctx
        .telemetry
        .as_deref()
        .map(|meter| meter.e2_lane_attempts().1)
        .unwrap_or_default();
    let hits = match retrieve(&request, spec).await {
        Ok(hits) => hits,
        Err(()) => {
            metrics.lane_elapsed_us = started.elapsed().as_micros();
            metrics.lane_leaf_attempts = lane_attempts(request.ctx, attempts_before);
            metrics.fail(FallbackStage::DateLane, FallbackReason::DateLeafError);
            return observed(primary, metrics, request.prepared, attempts);
        }
    };
    metrics.lane_leaf_attempts = lane_attempts(request.ctx, attempts_before);
    let candidates = match hydrate(hits, &request, spec, &mut metrics) {
        Some(candidates) => candidates,
        None => {
            metrics.lane_elapsed_us = started.elapsed().as_micros();
            metrics.fail(FallbackStage::ResourceLimit, FallbackReason::ResourceLimit);
            return observed(primary, metrics, request.prepared, attempts);
        }
    };
    metrics.lane_candidate_count = candidates.len();
    metrics.lane_candidate_bytes = charged_bytes(candidates.iter().map(|item| item.content.len()));
    metrics.lane_elapsed_us = started.elapsed().as_micros();
    metrics.lane_status = LaneStatus::Completed;
    let results = merge(primary, candidates, &mut metrics);
    observed(results, metrics, request.prepared, attempts)
}

fn observed(
    results: Vec<HybridSearchResult>,
    metrics: E2LaneMetrics,
    prepared: &PreparedOperation,
    primary_leaf_attempts: usize,
) -> LaneOutcome {
    LaneOutcome {
        results,
        observation: Some(metrics.observation(prepared, primary_leaf_attempts)),
    }
}

fn primary_attempts(ctx: &QueryContext) -> usize {
    ctx.telemetry
        .as_deref()
        .map(|meter| meter.e2_lane_attempts().0)
        .unwrap_or_default()
}

async fn retrieve(request: &LaneRequest<'_>, spec: &E2LaneSpec) -> Result<Vec<SearchResult>, ()> {
    let tantivy = request.state.tantivy.lock().await;
    let runner = Bm25Runner::date_lane(&tantivy, request.payload, request.ctx.telemetry.as_deref());
    let stream = request
        .ctx
        .stream_list
        .as_deref()
        .and_then(|streams| streams.first())
        .map(String::as_str);
    let leaf = match spec.inclusive_range {
        Some(range) => Bm25Leaf::date(&spec.query, stream, range, LANE_CANDIDATES_MAX),
        None => Bm25Leaf::plain(&spec.query, stream, LANE_CANDIDATES_MAX),
    };
    runner.run(leaf).map_err(|_| ())
}

fn lane_attempts(ctx: &QueryContext, before: usize) -> usize {
    ctx.telemetry
        .as_deref()
        .map(|meter| meter.e2_lane_attempts().1.saturating_sub(before))
        .unwrap_or_default()
}

fn hydrate(
    hits: Vec<SearchResult>,
    request: &LaneRequest<'_>,
    spec: &E2LaneSpec,
    metrics: &mut E2LaneMetrics,
) -> Option<Vec<HybridSearchResult>> {
    let mut candidates = Vec::new();
    let mut temporary_bytes = hits
        .capacity()
        .saturating_mul(std::mem::size_of::<SearchResult>())
        .saturating_add(
            hits.iter()
                .map(search_result_bytes)
                .fold(0usize, usize::saturating_add),
        );
    if hits.len() > LANE_CANDIDATES_MAX || temporary_bytes > TEMPORARY_BYTES_MAX {
        return None;
    }
    metrics.scan_temporary_bytes_upper_bound = temporary_bytes;
    for (rank, hit) in hits.into_iter().enumerate() {
        if let Some(result) = hydrate_one(hit, request, spec, metrics, &mut temporary_bytes)? {
            candidates.push((result, rank));
        }
    }
    metrics.scan_temporary_bytes_upper_bound = temporary_bytes;
    candidates.sort_by(|(left, left_rank), (right, right_rank)| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.id.cmp(&right.id))
            .then_with(|| left_rank.cmp(right_rank))
            .then_with(|| left.id.cmp(&right.id))
    });
    Some(candidates.into_iter().map(|(item, _)| item).collect())
}

fn search_result_bytes(result: &SearchResult) -> usize {
    result
        .id
        .capacity()
        .saturating_add(result.content.capacity())
        .saturating_add(result.user_id.capacity())
        .saturating_add(result.app_id.capacity())
        .saturating_add(result.stream.capacity())
}

fn eligible(chunk: &Chunk, request: &LaneRequest<'_>, spec: &E2LaneSpec) -> bool {
    live(chunk, request.payload)
        && in_scope(chunk, request.ctx)
        && agent_matches(chunk, request.payload)
        && bitemporal_matches(chunk, request.payload)
        && extraction_matches(chunk, request.payload)
        && event_date_matches(chunk, spec.inclusive_range)
}

fn live(chunk: &Chunk, payload: &SearchRequest) -> bool {
    !chunk.dormant
        && !chunk.in_progress
        && chunk.deleted_at.is_none()
        && (payload.include_superseded || (chunk.is_latest && chunk.superseded_by.is_none()))
}

fn in_scope(chunk: &Chunk, ctx: &QueryContext) -> bool {
    ctx.stream_list
        .as_ref()
        .is_none_or(|streams| streams.contains(&chunk.stream))
}

fn agent_matches(chunk: &Chunk, payload: &SearchRequest) -> bool {
    let agent = chunk.source.as_ref().map(|source| source.agent.as_str());
    payload
        .source_agent
        .as_deref()
        .is_none_or(|required| agent == Some(required))
        && payload
            .exclude_source_agents
            .as_ref()
            .is_none_or(|excluded| agent.is_none_or(|value| !excluded.iter().any(|x| x == value)))
}

fn bitemporal_matches(chunk: &Chunk, payload: &SearchRequest) -> bool {
    payload.valid_at.is_none_or(|at| {
        chunk.valid_from.is_none_or(|from| from <= at)
            && chunk.valid_until.is_none_or(|until| until >= at)
    })
}

fn extraction_matches(chunk: &Chunk, payload: &SearchRequest) -> bool {
    let filtered = payload.fact_type.is_some()
        || payload.subject.is_some()
        || payload.min_confidence.is_some();
    if !filtered {
        return true;
    }
    let Some(meta) = chunk.extraction_meta.as_ref() else {
        return false;
    };
    let kind = match meta.fact_type {
        FactType::PreferenceOrDecision => "preference_or_decision",
        FactType::ProjectState => "project_state",
        FactType::Fact => "fact",
        FactType::Event => "event",
        FactType::Experience => "experience",
    };
    payload
        .fact_type
        .as_ref()
        .is_none_or(|expected| expected == kind || meta.topic.as_ref() == Some(expected))
        && payload.subject.as_ref().is_none_or(|expected| {
            meta.subject
                .as_ref()
                .is_some_and(|actual| actual.to_lowercase().contains(&expected.to_lowercase()))
        })
        && payload
            .min_confidence
            .is_none_or(|minimum| meta.confidence >= minimum)
}

fn event_date_matches(chunk: &Chunk, range: Option<(i64, i64)>) -> bool {
    let Some((start, end)) = range else {
        return true;
    };
    let timestamp = chunk
        .extraction_meta
        .as_ref()
        .and_then(|meta| meta.event_date.as_deref())
        .and_then(|value| NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d").ok())
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .map(|date| date.and_utc().timestamp());
    timestamp.is_some_and(|value| (start..=end).contains(&value))
}

fn hybrid(hit: SearchResult, chunk: Chunk) -> Option<HybridSearchResult> {
    Some(HybridSearchResult {
        id: hit.id,
        content: chunk.content,
        user_id: hit.user_id,
        app_id: hit.app_id,
        level: chunk.level,
        timestamp: i64::try_from(chunk.timestamp).ok()?,
        score: f64::from(hit.score),
        bm25_score: hit.score,
        vector_score: 0.0,
        time_decay_factor: 1.0,
    })
}

pub(super) fn merge(
    primary: Vec<HybridSearchResult>,
    candidates: Vec<HybridSearchResult>,
    metrics: &mut E2LaneMetrics,
) -> Vec<HybridSearchResult> {
    let started = Instant::now();
    let count = primary.len();
    let input_count = count.saturating_add(candidates.len());
    let mut seen: HashSet<&str> = primary.iter().map(|item| item.id.as_str()).collect();
    if seen.len() != count || count > LANE_CANDIDATES_MAX {
        metrics.fail(
            FallbackStage::Validation,
            FallbackReason::InvariantViolation,
        );
        return primary;
    }
    let outsiders: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, item)| seen.insert(&item.id).then_some(index))
        .collect();
    metrics.merge_input_candidates = input_count;
    metrics.merge_unique_candidates = count.saturating_add(outsiders.len());
    metrics.merge_duplicate_candidates =
        input_count.saturating_sub(metrics.merge_unique_candidates);
    let admitted = outsiders
        .len()
        .min(ADMISSIONS_MAX)
        .min(count.saturating_sub(PROTECTED_PREFIX));
    // Conservative HashSet bucket/index/vector storage, with no cloned records.
    let merge_bytes = input_count
        .saturating_mul(128)
        .saturating_add(count.saturating_mul(std::mem::size_of::<HybridSearchResult>()));
    metrics.merge_temporary_bytes_upper_bound = merge_bytes;
    metrics.temporary_bytes_upper_bound = metrics
        .scan_temporary_bytes_upper_bound
        .saturating_add(merge_bytes);
    if metrics.temporary_bytes_upper_bound > TEMPORARY_BYTES_MAX {
        metrics.fail(FallbackStage::ResourceLimit, FallbackReason::ResourceLimit);
        return primary;
    }
    drop(seen);
    let mut output = Vec::with_capacity(count);
    let mut originals = primary.into_iter();
    output.extend(originals.by_ref().take(PROTECTED_PREFIX));
    output.extend(
        candidates
            .into_iter()
            .enumerate()
            .filter_map(|(index, item)| {
                outsiders
                    .iter()
                    .take(admitted)
                    .any(|i| *i == index)
                    .then_some(item)
            }),
    );
    output.extend(originals.take(count.saturating_sub(PROTECTED_PREFIX + admitted)));
    metrics.merge_elapsed_us = started.elapsed().as_micros();
    metrics.merge_output_candidates = output.len();
    metrics.actual_admissions = admitted;
    metrics.actual_removals = admitted;
    output
}

fn apply_preparation_fallback(metrics: &mut E2LaneMetrics, prepared: &PreparedOperation) {
    let Some(reason) = prepared.e2_fallback_reason() else {
        return;
    };
    metrics.fail(
        FallbackStage::DatePlan,
        match reason {
            "no_reference_time" => FallbackReason::NoReferenceTime,
            _ => FallbackReason::UnsupportedDateExpression,
        },
    );
}

pub(super) struct CandidateCredit {
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
        let next_scanned_bytes = metrics.scanned_bytes.checked_add(scan_bytes)?;
        if next_scanned_bytes > MAX_REQUEST_SCAN_BYTES {
            return None;
        }
        metrics.scanned_bytes = next_scanned_bytes;
        metrics.max_record_bytes = metrics.max_record_bytes.max(scan_bytes);
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

fn charge_hit(
    hit: &loomem_core::SearchResult,
    metrics: &mut super::operation_lane::E2LaneMetrics,
) -> Option<()> {
    if hit.content.len() > RECORD_SCAN_BYTES_MAX {
        return None;
    }
    let bytes = metrics.scanned_bytes.checked_add(hit.content.len())?;
    if bytes > REQUEST_SCAN_BYTES_MAX {
        return None;
    }
    metrics.scanned_bytes = bytes;
    metrics.scanned_candidates = metrics.scanned_candidates.saturating_add(1);
    Some(())
}

fn hydrate_one(
    hit: SearchResult,
    request: &LaneRequest<'_>,
    spec: &E2LaneSpec,
    metrics: &mut E2LaneMetrics,
    temporary_bytes: &mut usize,
) -> Option<Option<HybridSearchResult>> {
    charge_hit(&hit, metrics)?;
    let chunk = request.state.store.get_chunk(&hit.id).ok().flatten()?;
    charge_chunk(&chunk, metrics, temporary_bytes)?;
    if chunk.content.len() > RECORD_SCAN_BYTES_MAX {
        return None;
    }
    if eligible(&chunk, request, spec) {
        let result = hybrid(hit, chunk)?;
        *temporary_bytes = temporary_bytes
            .saturating_add(std::mem::size_of::<HybridSearchResult>())
            .saturating_add(hybrid_result_bytes(&result));
        if *temporary_bytes > TEMPORARY_BYTES_MAX {
            return None;
        }
        metrics.scan_temporary_bytes_upper_bound = *temporary_bytes;
        return Some(Some(result));
    }
    Some(None)
}

// Serialization into a counting sink bounds every metadata field without
// allocating a second copy of a potentially oversized record.
struct RecordBudget(usize);
impl std::io::Write for RecordBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let size = self.0.saturating_add(bytes.len());
        if size > RECORD_SCAN_BYTES_MAX {
            return Err(std::io::Error::other("record budget"));
        }
        self.0 = size;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn charge_chunk(chunk: &Chunk, metrics: &mut E2LaneMetrics, bytes: &mut usize) -> Option<()> {
    let mut record = RecordBudget(0);
    serde_json::to_writer(&mut record, chunk).ok()?;
    let scanned = metrics.scanned_bytes.checked_add(record.0)?;
    if scanned > REQUEST_SCAN_BYTES_MAX {
        return None;
    }
    metrics.scanned_bytes = scanned;
    *bytes = bytes
        .saturating_add(record.0.saturating_mul(4))
        .saturating_add(std::mem::size_of::<Chunk>());
    if *bytes > TEMPORARY_BYTES_MAX {
        return None;
    }
    metrics.scan_temporary_bytes_upper_bound = *bytes;
    Some(())
}

pub(super) fn intersect_ranges(left: (i64, i64), right: (i64, i64)) -> Option<(i64, i64)> {
    let range = (left.0.max(right.0), left.1.min(right.1));
    (range.0 <= range.1).then_some(range)
}

pub(super) fn hybrid_result_bytes(result: &loomem_core::HybridSearchResult) -> usize {
    result
        .id
        .capacity()
        .saturating_add(result.content.capacity())
        .saturating_add(result.user_id.capacity())
        .saturating_add(result.app_id.capacity())
}
