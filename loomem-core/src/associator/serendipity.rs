//! Serendipity score computation (3-signal formula).
//!
//! Sₑ = relevance × (1 - obviousness) × cluster_distance
//!
//! Three signals:
//! - relevance: how related the association is to the search context (top-5 results)
//! - obviousness: how directly similar the association is to the query itself
//! - cluster_distance: how far apart the association and query are in topic space
//!
//! PoC baseline (2-signal): spread = 0.13 (unusable)
//! Target (3-signal): spread > 0.40

use crate::associator::clustering::{centroid_distance, cosine_similarity};
use crate::storage::RocksDbStore;
use anyhow::Result;

/// A scored association candidate.
#[derive(Debug, Clone)]
pub struct ScoredAssociation {
    pub chunk_id: String,
    pub content: String,
    pub serendipity_score: f64,
    pub relevance: f64,
    pub obviousness: f64,
    pub cluster_dist: f64,
    pub source_mechanism: String,
}

/// Compute the serendipity score for an association candidate.
///
/// # Arguments
/// - `candidate_embedding`: embedding of the association candidate
/// - `query_embedding`: embedding of the original query
/// - `context_embeddings`: embeddings of the top-5 search results (context)
/// - `candidate_cluster`: cluster_id of the candidate
/// - `query_cluster`: cluster_id of the query (closest centroid)
/// - `store`: RocksDB store for centroid lookups
pub fn compute_serendipity(
    candidate_embedding: &[f32],
    query_embedding: &[f32],
    context_embeddings: &[&[f32]],
    candidate_cluster: u32,
    query_cluster: u32,
    store: &RocksDbStore,
) -> Result<SerendipityComponents> {
    // Relevance: max cosine similarity between candidate and any context chunk
    let relevance = context_embeddings
        .iter()
        .map(|ctx| cosine_similarity(candidate_embedding, ctx))
        .fold(0.0f64, f64::max)
        .max(0.0);

    // Obviousness: cosine similarity between candidate and query directly
    let obviousness = cosine_similarity(candidate_embedding, query_embedding).max(0.0);

    // Cluster distance: normalized distance between cluster centroids
    let cluster_dist = centroid_distance(store, candidate_cluster, query_cluster)?;

    // Sₑ = relevance × (1 - obviousness) × cluster_distance
    let se = relevance * (1.0 - obviousness) * cluster_dist;

    Ok(SerendipityComponents {
        score: se,
        relevance,
        obviousness,
        cluster_distance: cluster_dist,
    })
}

/// Components of a serendipity score for debugging/tracing.
#[derive(Debug, Clone)]
pub struct SerendipityComponents {
    pub score: f64,
    pub relevance: f64,
    pub obviousness: f64,
    pub cluster_distance: f64,
}

/// Find the closest cluster for a query embedding.
pub fn find_query_cluster(query_embedding: &[f32], store: &RocksDbStore) -> Result<Option<u32>> {
    let centroids = crate::associator::clustering::get_all_centroids(store)?;
    if centroids.is_empty() {
        return Ok(None);
    }

    let mut best_id = 0u32;
    let mut best_sim = f64::NEG_INFINITY;
    for (id, centroid) in &centroids {
        let sim = cosine_similarity(query_embedding, centroid);
        if sim > best_sim {
            best_sim = sim;
            best_id = *id;
        }
    }

    Ok(Some(best_id))
}

/// Validate that serendipity scores have sufficient spread for discrimination.
///
/// PoC baseline: 2-signal spread = 0.13 (unusable).
/// Target: spread > 0.40.
pub fn validate_spread(scores: &[f64]) -> SpreadResult {
    if scores.is_empty() {
        return SpreadResult {
            spread: 0.0,
            min: 0.0,
            max: 0.0,
            status: SpreadStatus::Fail,
        };
    }

    let min = scores.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = scores.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let spread = max - min;

    let status = if spread > 0.40 {
        SpreadStatus::Pass
    } else if spread > 0.30 {
        SpreadStatus::Warn
    } else {
        SpreadStatus::Fail
    };

    SpreadResult {
        spread,
        min,
        max,
        status,
    }
}

#[derive(Debug, Clone)]
pub struct SpreadResult {
    pub spread: f64,
    pub min: f64,
    pub max: f64,
    pub status: SpreadStatus,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SpreadStatus {
    /// spread > 0.40 — scoring discriminates well
    Pass,
    /// spread 0.30–0.40 — proceed with caution
    Warn,
    /// spread < 0.30 — scoring broken, investigate clusters
    Fail,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serendipity_obvious_match_near_zero() {
        // If candidate is identical to query, obviousness = 1.0, so Sₑ ≈ 0
        let query = vec![1.0, 0.0, 0.0];
        let candidate = vec![1.0, 0.0, 0.0]; // identical to query
        let context = [vec![0.8, 0.2, 0.0]];
        let _ctx_refs: Vec<&[f32]> = context.iter().map(|v| v.as_slice()).collect();

        // Same cluster → cluster_distance = 0.0, so Sₑ = 0
        // But even with different clusters, (1 - obviousness) ≈ 0
        let obviousness = cosine_similarity(&candidate, &query);
        assert!(obviousness > 0.99);
        // (1 - obviousness) ≈ 0, so Sₑ ≈ 0
    }

    #[test]
    fn test_serendipity_irrelevant_near_zero() {
        // If candidate is unrelated to context, relevance ≈ 0, so Sₑ ≈ 0
        let candidate = vec![0.0, 0.0, 1.0]; // orthogonal to context
        let context = [vec![1.0, 0.0, 0.0], vec![0.9, 0.1, 0.0]];
        let ctx_refs: Vec<&[f32]> = context.iter().map(|v| v.as_slice()).collect();

        let relevance = ctx_refs
            .iter()
            .map(|ctx| cosine_similarity(&candidate, ctx))
            .fold(0.0f64, f64::max);
        assert!(relevance < 0.1); // Very low relevance
    }

    #[test]
    fn test_validate_spread_pass() {
        let scores = vec![0.1, 0.3, 0.55, 0.7];
        let result = validate_spread(&scores);
        assert_eq!(result.status, SpreadStatus::Pass);
        assert!((result.spread - 0.6).abs() < 0.01);
    }

    #[test]
    fn test_validate_spread_fail() {
        // PoC-like scenario: narrow range
        let scores = vec![0.24, 0.30, 0.37];
        let result = validate_spread(&scores);
        assert_eq!(result.status, SpreadStatus::Fail);
        assert!((result.spread - 0.13).abs() < 0.01);
    }

    #[test]
    fn test_validate_spread_warn() {
        let scores = vec![0.2, 0.35, 0.52];
        let result = validate_spread(&scores);
        assert_eq!(result.status, SpreadStatus::Warn);
    }

    #[test]
    fn test_validate_spread_empty() {
        let result = validate_spread(&[]);
        assert_eq!(result.status, SpreadStatus::Fail);
    }
}
