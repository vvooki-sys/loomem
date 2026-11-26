//! Latest-state promotion (`[search] latest_state_boost`).
//!
//! Why: in the r5 LongMemEval holdout diagnosis, 7 of the 10 retrieval losses
//! in list_count / cross_session_dependency were the *later update* of a state
//! (withdrawn, finished, approved, signed off) missing from the context while
//! the earlier version of the same state was retrieved. The update fact exists
//! in the store but is less similar to the question than the earlier list or
//! plan it changes, so it ranks below the cut. `user_state_boost` cannot fix
//! this: it multiplies the old and the new version alike (same author, same
//! state type), and four of the seven updates are `Event` facts it never
//! boosts.
//!
//! What: for a query that asks about the *current* state (a fixed word list,
//! see [`asks_current_state`]), every positive candidate score is multiplied
//! by a recency factor from the *rank* of its statement time among the
//! distinct statement times in the pool: the oldest keeps `x1.0`, the newest
//! gets `x latest_state_boost`, the rest are spaced evenly between. Rank, not
//! linear time, so one outlier date (a wall-clock fallback, `9999-12-31`)
//! cannot squeeze every real date to one end. The statement time is the
//! conversation date of a server-owned ingest record (fact, source,
//! fragment), otherwise the chunk timestamp ([`statement_time`]). Superseded
//! chunks and chunks without a time stay neutral.
//!
//! Limits (A/B before enabling): it lifts *every* later statement in the pool,
//! whatever its topic; it reorders only the fused pool (3 x top_k), so an
//! update ranked below that pool cannot be rescued; Temporal-class queries
//! (re-sorted by timestamp later) and the cross-encoder reranker override
//! its order; graph-only and multi-query candidates added after it are not
//! promoted, so they lose ground relative to the pool.
//!
//! Default `latest_state_boost = 1.0` returns before any work (no store read,
//! no re-sort), so scores and ordering stay byte-identical.

use super::{HybridSearchEngine, HybridSearchResult};
use crate::storage::{
    is_ingest_fact_shape, is_ingest_fragment, is_ingest_source, Chunk, RocksDbStore,
};
use std::collections::BTreeSet;
use tracing::debug;

/// Single words that mark a question about the current state. Matched as
/// whole lowercase tokens (split on every non-alphanumeric character).
const STATE_WORDS: [&str; 22] = [
    "now",
    "currently",
    "current",
    "still",
    "latest",
    "anymore",
    "nowadays",
    "presently",
    "remaining",
    "teraz",
    "obecnie",
    "obecny",
    "obecna",
    "obecne",
    "aktualnie",
    "aktualny",
    "aktualna",
    "aktualne",
    "nadal",
    "wciąż",
    "wciaz",
    "dotychczas",
];

/// Multi-word markers, matched as consecutive whole tokens.
const STATE_PHRASES: [&[&str]; 11] = [
    &["so", "far"],
    &["as", "of"],
    &["ended", "up"],
    &["end", "up"],
    &["at", "the", "moment"],
    &["these", "days"],
    &["at", "present"],
    &["up", "to", "date"],
    &["any", "more"],
    &["do", "tej", "pory"],
    &["w", "tej", "chwili"],
];

/// True when `query` asks about the current state of something: it contains
/// one of [`STATE_WORDS`] or one of [`STATE_PHRASES`]. Case-insensitive,
/// whole tokens only ("snow" does not match "now"). Deterministic, no model
/// call.
///
/// The list is English-first with a few Polish markers (adverbs, the
/// `aktualny`/`obecny` adjective forms, `wciąż` with and without the
/// diacritic). It is a keyword heuristic: it has false positives ("current
/// account", "electric current") and misses current-state questions phrased
/// without a marker.
#[must_use]
pub fn asks_current_state(query: &str) -> bool {
    let lowered = query.to_lowercase();
    let tokens: Vec<&str> = lowered
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    tokens.iter().any(|token| STATE_WORDS.contains(token))
        || STATE_PHRASES
            .iter()
            .any(|phrase| tokens.windows(phrase.len()).any(|window| window == *phrase))
}

/// When the chunk's statement was made, as unix seconds. `None` for a
/// superseded chunk (`is_latest == false` or `superseded_by` set: never
/// promoted) and for a chunk without a usable time.
///
/// Order: the fragment's conversation date (server-owned fragment), then the
/// ingest conversation date (server-owned source or fact — the same gate as
/// the evidence renderer, so a client metadata key cannot forge it), then the
/// chunk timestamp when non-zero. A conversation date that is present but not
/// a canonical calendar `YYYY-MM-DD` yields `None`, not a timestamp fallback,
/// so one pool never mixes a bad date with a wall-clock time for the same
/// record.
#[must_use]
pub fn statement_time(chunk: &Chunk) -> Option<u64> {
    if !chunk.is_latest || chunk.superseded_by.is_some() {
        return None;
    }
    match fragment_date(chunk).or_else(|| ingest_date(chunk)) {
        Some(date) => day_start_unix(date),
        None => (chunk.timestamp > 0).then_some(chunk.timestamp),
    }
}

fn fragment_date(chunk: &Chunk) -> Option<&str> {
    if !is_ingest_fragment(chunk) {
        return None;
    }
    chunk.fragment.as_ref()?.conversation_date.as_deref()
}

fn ingest_date(chunk: &Chunk) -> Option<&str> {
    if !is_ingest_source(chunk) && !is_ingest_fact_shape(chunk) {
        return None;
    }
    chunk
        .metadata
        .as_ref()?
        .get("_loomem_ingest")?
        .get("conversation_date")?
        .as_str()
}

/// Midnight UTC of a canonical `YYYY-MM-DD` (the evidence renderer's rule:
/// formatting the parsed date must give the input back, so `2025-1-5` or a
/// padded value is rejected).
fn day_start_unix(date: &str) -> Option<u64> {
    let parsed = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    if parsed.format("%Y-%m-%d").to_string() != date {
        return None;
    }
    let seconds = parsed.and_hms_opt(0, 0, 0)?.and_utc().timestamp();
    u64::try_from(seconds).ok()
}

/// A count as `f64`, saturating at `u32::MAX` — a candidate pool never holds
/// that many distinct times.
fn count_f64(count: usize) -> f64 {
    f64::from(u32::try_from(count).unwrap_or(u32::MAX))
}

/// `1 + (boost - 1) * rank / (distinct - 1)`: `x1.0` for the oldest distinct
/// time, `x boost` for the newest. Callers guarantee `distinct >= 2`.
fn recency_factor(rank: usize, distinct: usize, boost: f64) -> f64 {
    let last = count_f64(distinct.saturating_sub(1)).max(1.0);
    let position = (count_f64(rank) / last).clamp(0.0, 1.0);
    1.0 + (boost - 1.0) * position
}

/// Multiply each positive result score by its [`recency_factor`] and
/// re-sort by score (stable, descending, `f64::total_cmp`). `time_of` maps a
/// result id to its statement time; `None` keeps that result's score
/// unchanged, and so does a score `<= 0.0` (a factor above 1 would push a
/// negative score *down*).
///
/// Returns `false` and leaves `results` untouched (scores and order) when the
/// pool has fewer than two distinct known times — equal timestamps carry no
/// "later" signal.
pub fn promote_by_recency<F>(results: &mut [HybridSearchResult], boost: f64, mut time_of: F) -> bool
where
    F: FnMut(&str) -> Option<u64>,
{
    let times: Vec<Option<u64>> = results.iter().map(|r| time_of(&r.id)).collect();
    let distinct: Vec<u64> = times
        .iter()
        .flatten()
        .copied()
        .collect::<BTreeSet<u64>>()
        .into_iter()
        .collect();
    if distinct.len() < 2 {
        return false;
    }
    for (result, time) in results.iter_mut().zip(&times) {
        let Some(time) = time else { continue };
        if result.score > 0.0 {
            let rank = distinct.partition_point(|known| known < time);
            result.score *= recency_factor(rank, distinct.len(), boost);
        }
    }
    results.sort_by(|a, b| b.score.total_cmp(&a.score));
    true
}

impl HybridSearchEngine {
    /// Apply `[search] latest_state_boost` to a fused candidate pool.
    ///
    /// No-op — no store read, no re-sort — unless the boost is a finite value
    /// above `1.0` and [`asks_current_state`] matches `query`. Otherwise each
    /// candidate's chunk is read once for its [`statement_time`] and the pool
    /// is promoted by [`promote_by_recency`].
    pub fn promote_latest_state(
        &self,
        query: &str,
        results: &mut [HybridSearchResult],
        store: &RocksDbStore,
    ) {
        let boost = self.config.search.latest_state_boost;
        if !boost.is_finite() || boost <= 1.0 || results.len() < 2 || !asks_current_state(query) {
            return;
        }
        let promoted = promote_by_recency(results, boost, |id| {
            store
                .get_chunk(id)
                .ok()
                .flatten()
                .as_ref()
                .and_then(statement_time)
        });
        debug!(
            promoted,
            candidates = results.len(),
            "latest-state promotion"
        );
    }
}

#[cfg(test)]
mod tests;
