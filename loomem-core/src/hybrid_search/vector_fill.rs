//! Real-cosine fill for fused candidates outside the vector lane
//! (`[search] vector_fill_missing`).
//!
//! Why: the BM25 lane keeps the top `2 x request top_k` hits, the vector lane
//! the top `2 x max(config top_k, request top_k)` cosines, and fusion keeps
//! `3 x max(...)`. A candidate that is in the BM25 lane but below the vector
//! lane's cut used to get `vector_score = 0.0`, not its real cosine (in the
//! r5 holdout diagnosis ~0.75-0.80 against a lane max of ~0.87-0.90), so its
//! fused score was `bm25 weight x bm25_n` alone and it sank to the bottom of
//! the pool. Three of the ten diagnosed missing "update" facts were exactly
//! this case.
//!
//! What: for every BM25-lane id (rare-term-lane candidates join the BM25
//! lane first) that is not a vector-lane hit, take the cosine between the
//! query embedding and that chunk's embedding from the *same* embedding list
//! the vector lane ranked (no storage read). The raw cosine passes the same
//! `vector_similarity_floor` as lane hits, is divided by the lane's own max
//! (computed before the fill, so the fill cannot move it) and clamped to
//! [0, 1]: a fill never scores above the best lane hit and never below the
//! old 0.0. No embedding, an empty lane or a non-positive lane max leaves
//! 0.0.
//!
//! Limits (A/B before enabling): it only rescores candidates that are
//! already in the pool through a non-vector lane; a fact in no lane (6 of
//! the 10 diagnosed cases, all of which stayed out of the top 60 in the
//! offline check) stays out. It lifts every such candidate, stale ones
//! included, and so can push other candidates down (widening the vector
//! lane in the diagnosis moved a stale fact from rank 11 to 42). With
//! `decay_match_relief` > 0 (0.70 shipped) a filled vector score also feeds
//! `relieve_decay(max(bm25, vector))`, so a filled older candidate loses part
//! of its age penalty too: a second way the flag can lift stale facts. It
//! costs one pass over the in-memory embedding list plus one cosine per
//! filled candidate. Vector-only hits get no BM25 fill. BM25 sub-query hits
//! of `multi_query_enabled` join the BM25 lane before fusion and are filled;
//! candidates added after fusion (graph, vector multi-query, operation lane)
//! are not. It applies wherever `search_with_vector*` fuses: `/v1/search`
//! (and MCP `memory_search`, which calls the same handler) and the hosted
//! compat search; the ambient path fuses through `fuse_with_vector` and is
//! not filled.
//!
//! Default `vector_fill_missing = false` computes nothing: the fusion input
//! is unchanged and output is byte-identical.

use std::collections::{HashMap, HashSet};

use super::channels::Bm25Map;
use crate::tantivy_index::SearchResult;
use crate::vector_search::cosine_similarity;
use tracing::debug;

/// The vector lane after the floor filter, as fusion normalizes it.
pub(super) struct LaneScale {
    /// Lane max raw cosine (the fusion normalizer).
    pub max_vector: f32,
    /// `vector_similarity_floor` (0.0 == off).
    pub floor: f64,
    /// No lane hit survived the floor.
    pub lane_empty: bool,
}

/// Raw cosines between `query_embedding` and the embeddings of BM25-lane
/// ids that are not vector-lane hits. Ids without an entry in
/// `all_embeddings` are absent from the result (they keep 0.0).
pub(super) fn missing_cosines(
    bm25_results: &[SearchResult],
    vector_lane: &[(String, f32)],
    all_embeddings: &[(String, Vec<f32>)],
    query_embedding: &[f32],
) -> HashMap<String, f32> {
    let in_lane: HashSet<&str> = vector_lane.iter().map(|(id, _)| id.as_str()).collect();
    let missing: HashSet<&str> = bm25_results
        .iter()
        .map(|r| r.id.as_str())
        .filter(|id| !in_lane.contains(id))
        .collect();
    if missing.is_empty() || query_embedding.is_empty() {
        return HashMap::new();
    }
    all_embeddings
        .iter()
        .filter(|(id, _)| missing.contains(id.as_str()))
        .map(|(id, embedding)| (id.clone(), cosine_similarity(query_embedding, embedding)))
        .collect()
}

/// Normalized vector score for a filled raw cosine: floor as for lane hits,
/// divided by the lane max, clamped to [0, 1].
pub(super) fn filled_score(raw: f32, scale: &LaneScale) -> f32 {
    let bad_max = scale.max_vector <= 0.0 || !scale.max_vector.is_finite();
    if scale.lane_empty || bad_max || !raw.is_finite() {
        return 0.0;
    }
    if scale.floor > 0.0 && f64::from(raw) < scale.floor {
        return 0.0;
    }
    (raw / scale.max_vector).clamp(0.0, 1.0)
}

/// Insert filled scores for BM25 ids that have no vector-lane score.
/// Returns how many candidates got a non-zero fill.
pub(super) fn apply(
    vector_map: &mut HashMap<String, f32>,
    fill: &HashMap<String, f32>,
    bm25_map: &Bm25Map,
    scale: &LaneScale,
) -> usize {
    let mut filled = 0_usize;
    for (id, raw) in fill {
        if !bm25_map.contains_key(id) || vector_map.contains_key(id) {
            continue;
        }
        let score = filled_score(*raw, scale);
        if score > 0.0 {
            vector_map.insert(id.clone(), score);
            filled += 1;
        }
    }
    debug!(
        filled,
        candidates = fill.len(),
        "vector_fill_missing: filled vector scores"
    );
    filled
}

#[cfg(test)]
mod tests;
