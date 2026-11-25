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
//! see [`asks_current_state`]), every candidate's score is multiplied by a
//! recency factor that grows linearly with when its statement was made,
//! relative to the candidate pool: the oldest statement keeps `x1.0`, the
//! newest gets `x latest_state_boost`. The statement time is the conversation
//! date of a server-owned ingest record (fact, source, fragment), otherwise the
//! chunk timestamp ([`statement_time`]). Superseded chunks and chunks without
//! a time stay neutral.
//!
//! Default `latest_state_boost = 1.0` returns before any work (no store read,
//! no re-sort), so scores and ordering stay byte-identical.

use super::{HybridSearchEngine, HybridSearchResult};
use crate::storage::{
    is_ingest_fact_shape, is_ingest_fragment, is_ingest_source, Chunk, RocksDbStore,
};
use tracing::debug;

/// Single words that mark a question about the current state. Matched as
/// whole lowercase tokens (split on every non-alphanumeric character).
const STATE_WORDS: [&str; 15] = [
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
    "aktualnie",
    "nadal",
    "wciąż",
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
/// one of [`STATE_WORDS`] ("now", "currently", "still", "latest", …, plus the
/// Polish "teraz", "obecnie", "aktualnie", "nadal", "wciąż", "dotychczas") or
/// one of [`STATE_PHRASES`] ("so far", "as of", "ended up", "at the moment",
/// "do tej pory", …). Case-insensitive, whole tokens only ("snow" does not
/// match "now"). Deterministic, no model call.
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
/// a calendar `YYYY-MM-DD` yields `None`, not a timestamp fallback, so one
/// pool never mixes a bad date with a wall-clock time for the same record.
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

fn day_start_unix(date: &str) -> Option<u64> {
    let seconds = chrono::NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)?
        .and_utc()
        .timestamp();
    u64::try_from(seconds).ok()
}

/// Seconds as `f64`, saturating at `u32::MAX` (~136 years) — far beyond any
/// real pool span; a saturated span only flattens the factor toward `boost`.
fn seconds_f64(delta: u64) -> f64 {
    f64::from(u32::try_from(delta).unwrap_or(u32::MAX))
}

/// `1 + (boost - 1) * position`, `position = since_oldest / span` in `[0, 1]`.
fn recency_factor(since_oldest: u64, span: u64, boost: f64) -> f64 {
    let position = (seconds_f64(since_oldest) / seconds_f64(span)).clamp(0.0, 1.0);
    1.0 + (boost - 1.0) * position
}

/// Multiply each result's score by its [`recency_factor`] within the pool and
/// re-sort by score (stable, descending). `time_of` maps a result id to its
/// statement time; `None` keeps that result's score unchanged.
///
/// Returns `false` and leaves `results` untouched (scores and order) when the
/// pool has fewer than two distinct known times — equal timestamps carry no
/// "later" signal.
pub fn promote_by_recency<F>(results: &mut [HybridSearchResult], boost: f64, mut time_of: F) -> bool
where
    F: FnMut(&str) -> Option<u64>,
{
    let times: Vec<Option<u64>> = results.iter().map(|r| time_of(&r.id)).collect();
    let oldest = times.iter().flatten().min().copied();
    let newest = times.iter().flatten().max().copied();
    let (Some(oldest), Some(newest)) = (oldest, newest) else {
        return false;
    };
    if newest <= oldest {
        return false;
    }
    let span = newest - oldest;
    for (result, time) in results.iter_mut().zip(&times) {
        if let Some(time) = time {
            result.score *= recency_factor(time - oldest, span, boost);
        }
    }
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
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
