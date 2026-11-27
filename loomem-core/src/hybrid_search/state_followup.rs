//! State follow-up lane (`[search] state_followup_*`).
//!
//! Why: in the offline diagnosis of the r5 LongMemEval holdout (§ B.15), the
//! facts that *update* a state ("Ashdown's ottoman was finished and
//! collected") are lexically and semantically far from the question ("who
//! is on the waiting list") and never reach the top 60 candidates, while the
//! stale fact that *is* in the top-k is close to the update. Querying with
//! the stale fact's own text pulled the update into the top 60 in 8 of the 9
//! measurable cases.
//!
//! What: after the first hybrid fusion, the server takes the requested
//! top-k of the fused pool in rank order and picks up to `max_seeds` "state
//! facts" ([`HybridSearchEngine::state_followup_seeds`]): chunks whose
//! `extraction_meta.fact_type` is in `fact_types`, that are `is_latest`,
//! not superseded and not soft-deleted. Each seed's *stored* vector is
//! reused as the follow-up query (looked up in the filtered embedding list,
//! else read from the store; only a seed without a stored vector is embedded
//! afresh with the query embedder, so chunk content normally never leaves
//! the process; no LLM) and ranked against the already filtered in-memory
//! embedding list; the resulting hits are merged into the fused pool
//! ([`HybridSearchEngine::merge_state_followup`]): a hit that is the seed
//! itself, already in the pool or soft-deleted is skipped (existing
//! candidates keep their exact scores), a hit below
//! `vector_similarity_floor` is skipped,
//! the rest are normalized by the best non-seed cosine of that seed's hit
//! list (the seed's self-similarity would otherwise make the normalization
//! a no-op), scored like a vector-only fusion hit (`hybrid_weights.vector x
//! normalized cosine x state_followup_weight`, then the same decay relief,
//! trust/provenance and attribution multipliers as fusion), appended, and
//! the pool is re-sorted and re-cut to `3 x max(top_k, request top_k)`. A
//! chunk reached from several seeds is added once, by the first seed in
//! rank order. Follow-up candidates are not guaranteed: they compete for the
//! pool like any other candidate. For a follow-up candidate
//! `HybridSearchResult::vector_score` holds the raw cosine to its seed
//! (fusion candidates hold the max-normalized cosine to the query).
//!
//! Limits (A/B before enabling): it lifts every neighbour of a state fact,
//! stale or not, and the re-cut can push lower-ranked old candidates out of
//! the pool, so the top-k membership can change. It costs up to `max_seeds`
//! in-memory scans per request, plus one embedder call for each seed whose
//! stored vector is missing (measured offline on the r5 copies, see the
//! PR). The server skips it under a date filter
//! unless `with_date_filter` is set; the embedding list it scans is the
//! stream/date/agent-filtered list of the main query, so a follow-up under
//! a date filter inherits that filter. It runs for `/v1/search` and MCP
//! `memory_search` (same handler), not for the hosted compat search or the
//! ambient path.
//!
//! Default `state_followup_enabled = false` runs nothing: no store read, no
//! embedding, no re-sort; scores and ordering stay byte-identical.

use std::collections::HashSet;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::debug;

use super::{
    retain_pool_with_guaranteed, HybridSearchEngine, HybridSearchResult, SearchCandidateBudget,
};
use crate::storage::{Chunk, FactType, RocksDbStore};

/// The six flat `[search] state_followup_*` keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateFollowupConfig {
    /// Master switch. `false` (default) runs nothing.
    #[serde(default, rename = "state_followup_enabled")]
    pub enabled: bool,
    /// Seeds taken in rank order from the requested top-k of the fused pool.
    #[serde(default = "default_max_seeds", rename = "state_followup_max_seeds")]
    pub max_seeds: usize,
    /// Vector hits fetched per seed.
    #[serde(
        default = "default_hits_per_seed",
        rename = "state_followup_hits_per_seed"
    )]
    pub hits_per_seed: usize,
    /// `extraction_meta.fact_type` values that qualify as a seed.
    #[serde(default = "default_fact_types", rename = "state_followup_fact_types")]
    pub fact_types: Vec<FactType>,
    /// Multiplier on the follow-up vector score. Finite and `> 0`.
    #[serde(default = "default_weight", rename = "state_followup_weight")]
    pub weight: f64,
    /// Run the step when the request carries a date filter too. `false`
    /// (default) skips it under a date filter.
    #[serde(default, rename = "state_followup_with_date_filter")]
    pub with_date_filter: bool,
}

fn default_max_seeds() -> usize {
    3
}

fn default_hits_per_seed() -> usize {
    60
}

fn default_fact_types() -> Vec<FactType> {
    vec![FactType::ProjectState, FactType::Event]
}

fn default_weight() -> f64 {
    1.0
}

impl Default for StateFollowupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_seeds: default_max_seeds(),
            hits_per_seed: default_hits_per_seed(),
            fact_types: default_fact_types(),
            weight: default_weight(),
            with_date_filter: false,
        }
    }
}

impl StateFollowupConfig {
    /// Reject values that could never be meant: zero counts, a non-finite
    /// or non-positive weight, and an empty type list while enabled.
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.max_seeds >= 1,
            "search.state_followup_max_seeds must be positive"
        );
        anyhow::ensure!(
            self.hits_per_seed >= 1,
            "search.state_followup_hits_per_seed must be positive"
        );
        anyhow::ensure!(
            self.weight.is_finite() && self.weight > 0.0,
            "search.state_followup_weight must be a finite number > 0.0"
        );
        anyhow::ensure!(
            !self.enabled || !self.fact_types.is_empty(),
            "search.state_followup_fact_types must not be empty when state_followup_enabled = true"
        );
        Ok(())
    }
}

/// A state fact picked from the fused pool; `content` is what gets embedded.
#[derive(Debug, Clone, PartialEq)]
pub struct Seed {
    pub id: String,
    pub content: String,
}

/// The vector hits of one seed: `(chunk id, raw cosine to the seed)`, as
/// `vector_search::vector_search` returns them.
#[derive(Debug, Clone)]
pub struct SeedHits {
    pub seed_id: String,
    pub hits: Vec<(String, f32)>,
}

/// What the merge did, for diagnostics.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StateFollowupOutcome {
    /// Seed ids in the order their hits were merged.
    pub seeds: Vec<String>,
    /// Follow-up candidates that are in the pool after the re-cut.
    pub injected_ids: Vec<String>,
}

/// Store and pool budget the merge works against.
pub struct FollowupPool<'a> {
    pub store: &'a RocksDbStore,
    pub budget: SearchCandidateBudget,
    pub guaranteed: Option<&'a HashSet<String>>,
}

/// Whether `chunk` can seed a follow-up: a configured fact type, latest, not
/// superseded, not soft-deleted (a soft-deleted chunk can still sit in the
/// pool while its delete chain is incomplete; its text must not be used).
fn is_seed(chunk: &Chunk, fact_types: &[FactType]) -> bool {
    let typed = chunk
        .extraction_meta
        .as_ref()
        .is_some_and(|meta| fact_types.contains(&meta.fact_type));
    typed && chunk.is_latest && chunk.superseded_by.is_none() && chunk.deleted_at.is_none()
}

/// Best cosine of a seed's hit list after the floor, the seed itself
/// excluded. `None` when nothing qualifies or the max is not positive.
fn seed_scale(seed: &SeedHits, floor: f64) -> Option<f32> {
    let max = seed
        .hits
        .iter()
        .filter(|(id, cosine)| *id != seed.seed_id && passes_floor(*cosine, floor))
        .map(|(_, cosine)| *cosine)
        .fold(f32::NEG_INFINITY, f32::max);
    (max.is_finite() && max > 0.0).then_some(max)
}

fn passes_floor(cosine: f32, floor: f64) -> bool {
    cosine.is_finite() && (floor <= 0.0 || f64::from(cosine) >= floor)
}

impl HybridSearchEngine {
    /// Up to `max_seeds` seeds from the first `limit` (requested top-k)
    /// results, in rank order. Empty, with no store read, when the step is
    /// disabled.
    pub fn state_followup_seeds(
        &self,
        results: &[HybridSearchResult],
        limit: usize,
        store: &RocksDbStore,
    ) -> Vec<Seed> {
        let cfg = &self.config.search.state_followup;
        if !cfg.enabled {
            return Vec::new();
        }
        results
            .iter()
            .take(limit)
            .filter_map(|result| store.get_chunk(&result.id).ok().flatten())
            .filter(|chunk| is_seed(chunk, &cfg.fact_types))
            .take(cfg.max_seeds)
            .map(|chunk| Seed {
                id: chunk.id,
                content: chunk.content,
            })
            .collect()
    }

    /// Merge the seeds' vector hits into the fused pool (see the module
    /// docs), then re-sort and re-cut to `pool.budget.pool_size()`.
    /// Existing candidates keep their exact scores; only new ids are added.
    pub fn merge_state_followup(
        &self,
        results: &mut Vec<HybridSearchResult>,
        seed_hits: Vec<SeedHits>,
        pool: FollowupPool<'_>,
    ) -> StateFollowupOutcome {
        let cfg = &self.config.search.state_followup;
        if !cfg.enabled || seed_hits.is_empty() {
            return StateFollowupOutcome::default();
        }
        let floor = self.config.search.vector_similarity_floor;
        let mut present: HashSet<String> = results.iter().map(|r| r.id.clone()).collect();
        let mut added: Vec<String> = Vec::new();
        let mut seeds = Vec::with_capacity(seed_hits.len());
        for seed in &seed_hits {
            seeds.push(seed.seed_id.clone());
            let Some(max_cosine) = seed_scale(seed, floor) else {
                continue;
            };
            for (id, cosine) in &seed.hits {
                if *id == seed.seed_id || present.contains(id) || !passes_floor(*cosine, floor) {
                    continue;
                }
                let Some(result) = self.followup_result(id, *cosine, max_cosine, pool.store) else {
                    continue;
                };
                present.insert(id.clone());
                added.push(id.clone());
                results.push(result);
            }
        }
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        retain_pool_with_guaranteed(results, pool.budget.pool_size(), pool.guaranteed);
        let kept: HashSet<&str> = results.iter().map(|r| r.id.as_str()).collect();
        let injected_ids: Vec<String> = added
            .into_iter()
            .filter(|id| kept.contains(id.as_str()))
            .collect();
        debug!(
            seeds = seeds.len(),
            injected = injected_ids.len(),
            pool = results.len(),
            "state_followup: merged seed hits"
        );
        StateFollowupOutcome {
            seeds,
            injected_ids,
        }
    }

    /// One follow-up candidate, scored like a vector-only fusion hit with the
    /// follow-up weight applied to the fusion term. `None` when the chunk is
    /// not in the store or is soft-deleted.
    fn followup_result(
        &self,
        id: &str,
        cosine: f32,
        max_cosine: f32,
        store: &RocksDbStore,
    ) -> Option<HybridSearchResult> {
        let chunk = store.get_chunk(id).ok().flatten()?;
        if chunk.deleted_at.is_some() {
            return None;
        }
        // `fuse_lanes` wraps this with `as`; a timestamp above `i64::MAX` is
        // practically unreachable, so the candidate is simply skipped.
        let timestamp = i64::try_from(chunk.timestamp).ok()?;
        let normalized = (cosine / max_cosine).clamp(0.0, 1.0);
        let fusion_score = self.config.search.hybrid_weights.vector
            * f64::from(normalized)
            * self.config.search.state_followup.weight;
        let scored = self.finish_score(
            fusion_score,
            normalized,
            chunk.level,
            timestamp,
            Some(&chunk),
        );
        Some(HybridSearchResult {
            id: id.to_string(),
            content: chunk.content,
            user_id: "default".to_string(),
            app_id: "default".to_string(),
            level: chunk.level,
            timestamp,
            score: scored.score,
            bm25_score: 0.0,
            vector_score: cosine,
            time_decay_factor: scored.time_decay,
        })
    }
}

#[cfg(test)]
mod tests;
