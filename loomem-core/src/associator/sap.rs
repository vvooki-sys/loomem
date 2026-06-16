//! Semantic Adjacent Possible (ECA-19).
//!
//! Finds chunks that are semantically close to the boundary of the query's cluster
//! but in a different cluster. These "adjacent possible" chunks represent ideas
//! that are related to the context but approach it from a different angle.

use anyhow::Result;
use std::sync::Arc;
use tracing::debug;

use crate::associator::clustering::{cosine_similarity, get_all_centroids, get_cluster_id};
use crate::storage::RocksDbStore;

/// A candidate from the adjacent possible.
#[derive(Debug, Clone)]
pub struct AdjacentCandidate {
    pub chunk_id: String,
    pub score: f64,
    pub cluster_id: u32,
}

/// Find chunks in the "adjacent possible" — related to context but from different clusters.
///
/// Algorithm:
/// 1. Compute centroid of top-5 result embeddings
/// 2. Find clusters near the query cluster (but not the same)
/// 3. Within those clusters, find chunks with:
///    - cosine(chunk, centroid) > relevance_min (related to context)
///    - cosine(chunk, query) < obviousness_max (not obvious match to query)
///
/// # Arguments
/// - `store`: RocksDB store
/// - `query_embedding`: embedding of the original query
/// - `context_embeddings`: embeddings of the top-5 search results
/// - `query_cluster`: cluster_id of the query
/// - `count`: max results to return
/// - `relevance_min`: minimum cosine to context centroid (default 0.3)
/// - `obviousness_max`: maximum cosine to query (default 0.6)
pub fn find_adjacent_possible(
    store: &Arc<RocksDbStore>,
    query_embedding: &[f32],
    context_embeddings: &[Vec<f32>],
    query_cluster: u32,
    count: usize,
    relevance_min: f64,
    obviousness_max: f64,
    stream_id: &str,
) -> Result<Vec<AdjacentCandidate>> {
    if context_embeddings.is_empty() {
        return Ok(Vec::new());
    }

    let dim = context_embeddings[0].len();

    // Step 1: Compute centroid of context embeddings
    let mut centroid = vec![0.0f32; dim];
    for emb in context_embeddings {
        for (i, v) in emb.iter().enumerate() {
            centroid[i] += v;
        }
    }
    let n = context_embeddings.len() as f32;
    for v in centroid.iter_mut() {
        *v /= n;
    }
    // Normalize
    let norm: f32 = centroid.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in centroid.iter_mut() {
            *v /= norm;
        }
    }

    // Step 2: Find nearby clusters (by centroid distance to context centroid)
    let all_centroids = get_all_centroids(store)?;
    let mut nearby_clusters: Vec<(u32, f64)> = all_centroids
        .iter()
        .filter(|(id, _)| *id != query_cluster)
        .map(|(id, c)| (*id, cosine_similarity(&centroid, c)))
        .filter(|(_, sim)| *sim > 0.2) // At least somewhat related
        .collect();
    nearby_clusters.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    nearby_clusters.truncate(5); // Top 5 nearby clusters

    if nearby_clusters.is_empty() {
        return Ok(Vec::new());
    }

    // Step 3: Scan embeddings in nearby clusters
    let all_embeddings = store.get_all_embeddings()?;
    let mut candidates: Vec<AdjacentCandidate> = Vec::new();

    for (chunk_id, emb) in &all_embeddings {
        // Stream + quality filter
        if let Ok(Some(chunk)) = store.get_chunk(chunk_id) {
            if !crate::associator::is_associable_in_stream(&chunk, stream_id) {
                continue;
            }
        } else {
            continue;
        }

        // Check cluster membership
        let cluster = match get_cluster_id(store, chunk_id)? {
            Some(c) => c,
            None => continue,
        };

        // Must be in a nearby cluster (not the query cluster)
        if !nearby_clusters.iter().any(|(id, _)| *id == cluster) {
            continue;
        }

        // Check relevance to context
        let relevance = cosine_similarity(emb, &centroid);
        if relevance < relevance_min {
            continue;
        }

        // Check it's not too obvious
        let obviousness = cosine_similarity(emb, query_embedding);
        if obviousness > obviousness_max {
            continue;
        }

        // Score: relevance * (1 - obviousness) — higher is better
        let score = relevance * (1.0 - obviousness);

        candidates.push(AdjacentCandidate {
            chunk_id: chunk_id.clone(),
            score,
            cluster_id: cluster,
        });
    }

    // Sort by score descending, take top `count`
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.truncate(count);

    debug!(
        "Adjacent possible: found {} candidates from {} nearby clusters",
        candidates.len(),
        nearby_clusters.len()
    );

    Ok(candidates)
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_centroid_computation() {
        let embeddings = vec![vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]];
        let dim = 3;
        let mut centroid = vec![0.0f32; dim];
        for emb in &embeddings {
            for (i, v) in emb.iter().enumerate() {
                centroid[i] += v;
            }
        }
        let n = embeddings.len() as f32;
        for v in centroid.iter_mut() {
            *v /= n;
        }
        assert!((centroid[0] - 0.5).abs() < 0.01);
        assert!((centroid[1] - 0.5).abs() < 0.01);
        assert!((centroid[2]).abs() < 0.01);
    }
}
