//! Per-channel score normalization for the weighted BM25 + vector fusion
//! (moved unchanged out of `fuse_with_vector_budget`), plus the optional
//! `vector_fill_missing` step on the normalized vector map.
use std::collections::HashMap;

use super::vector_fill::{self, LaneScale};
use crate::tantivy_index::SearchResult;

/// BM25 id -> (max-normalized score, hit).
pub(super) type Bm25Map = HashMap<String, (f32, SearchResult)>;

/// Raw inputs of one fusion: the BM25 lane, the vector lane (raw cosines),
/// and the raw cosines of BM25 ids missing from the vector lane (`None`
/// unless `[search] vector_fill_missing` is on).
pub(super) struct Lanes {
    pub bm25: Vec<SearchResult>,
    pub vector: Vec<(String, f32)>,
    pub fill: Option<HashMap<String, f32>>,
}

/// Max-normalize each channel. With `lanes.fill == None` this is exactly
/// the pre-fill computation (same floor filter, maxima and maps).
pub(super) fn normalized_maps(lanes: Lanes, floor: f64) -> (Bm25Map, HashMap<String, f32>) {
    let Lanes {
        bm25: bm25_results,
        vector: vector_scores,
        fill,
    } = lanes;
    // WS-1(a) (Spectron gap brief 2026-07-09): absolute-similarity floor.
    // Drop vector candidates whose *raw* cosine sits below the floor
    // before max-normalization — otherwise the top hit of a noise-only
    // vector channel is normalized to 1.0 and still collects the full
    // `weights.vector`. Dropped candidates can still enter via BM25.
    // floor == 0.0 (default) → byte-identical to the pre-cycle path.
    let vector_scores: Vec<(String, f32)> = if floor > 0.0 {
        vector_scores
            .into_iter()
            .filter(|(_, score)| f64::from(*score) >= floor)
            .collect()
    } else {
        vector_scores
    };

    // Normalize BM25 scores
    let max_bm25 = bm25_results
        .iter()
        .map(|r| r.score)
        .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or(1.0);

    // Normalize vector scores (cosine similarity is typically -1 to 1, shift to 0-1)
    let max_vector = vector_scores
        .iter()
        .map(|r| r.1)
        .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or(1.0);
    let lane_empty = vector_scores.is_empty();

    // Build maps for efficient lookup
    let mut bm25_map: Bm25Map = HashMap::new();
    for result in bm25_results {
        let normalized = result.score / max_bm25;
        bm25_map.insert(result.id.clone(), (normalized, result));
    }

    let mut vector_map: HashMap<String, f32> = HashMap::new();
    for (id, score) in vector_scores {
        let normalized = if max_vector > 0.0 {
            score / max_vector
        } else {
            0.0
        };
        vector_map.insert(id, normalized);
    }

    if let Some(fill) = fill {
        let scale = LaneScale {
            max_vector,
            floor,
            lane_empty,
        };
        vector_fill::apply(&mut vector_map, &fill, &bm25_map, &scale);
    }
    (bm25_map, vector_map)
}
