//! Topic clustering via K-means on chunk embeddings.
//!
//! Assigns each chunk to a topic cluster and stores centroids.
//! Used by serendipity scoring, adjacent possible, and dream mode.

use anyhow::{Context, Result};
use std::sync::Arc;
use tracing::{debug, info};

use serde::{Deserialize, Serialize};

use crate::config::AssociatorConfig;
use crate::storage::RocksDbStore;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusteringConfig {
    pub interval_secs: u64,
    pub max_iterations: usize,
    pub timeout_secs: u64,
}

/// Cosine similarity between two vectors (reused from consolidation.rs pattern).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    (dot / (norm_a * norm_b)) as f64
}

/// Result of a clustering run.
#[derive(Debug)]
pub struct ClusteringResult {
    pub num_clusters: usize,
    pub num_chunks: usize,
    pub iterations: usize,
}

/// Get the cluster_id for a chunk from RocksDB.
pub fn get_cluster_id(store: &RocksDbStore, chunk_id: &str) -> Result<Option<u32>> {
    let key = format!("assoc:cluster:{}", chunk_id);
    match store.get(key.as_bytes())? {
        Some(bytes) => {
            let s = String::from_utf8_lossy(&bytes);
            Ok(Some(s.parse::<u32>().unwrap_or(0)))
        }
        None => Ok(None),
    }
}

/// Get a cluster centroid from RocksDB.
pub fn get_centroid(store: &RocksDbStore, cluster_id: u32) -> Result<Option<Vec<f32>>> {
    let key = format!("assoc:centroid:{}", cluster_id);
    match store.get(key.as_bytes())? {
        Some(bytes) => {
            let centroid: Vec<f32> =
                bincode::deserialize(&bytes).context("Failed to deserialize centroid")?;
            Ok(Some(centroid))
        }
        None => Ok(None),
    }
}

/// Get all centroids from RocksDB.
pub fn get_all_centroids(store: &RocksDbStore) -> Result<Vec<(u32, Vec<f32>)>> {
    let prefix = b"assoc:centroid:";
    let mut centroids = Vec::new();
    for (key, value) in store.prefix_scan(prefix) {
        let key_str = String::from_utf8_lossy(&key);
        if let Some(id_str) = key_str.strip_prefix("assoc:centroid:") {
            if let Ok(id) = id_str.parse::<u32>() {
                if let Ok(centroid) = bincode::deserialize::<Vec<f32>>(&value) {
                    centroids.push((id, centroid));
                }
            }
        }
    }
    Ok(centroids)
}

/// Compute normalized distance between two cluster centroids (0–1).
/// Returns 0.0 if same cluster, 1.0 if maximally different.
pub fn centroid_distance(store: &RocksDbStore, cluster_a: u32, cluster_b: u32) -> Result<f64> {
    if cluster_a == cluster_b {
        return Ok(0.0);
    }
    let ca = get_centroid(store, cluster_a)?;
    let cb = get_centroid(store, cluster_b)?;
    match (ca, cb) {
        (Some(a), Some(b)) => {
            let sim = cosine_similarity(&a, &b);
            // Convert similarity to distance: 1.0 - sim, clamped to [0, 1]
            Ok((1.0 - sim).clamp(0.0, 1.0))
        }
        _ => Ok(0.5), // Unknown clusters → neutral distance
    }
}

/// Run K-means clustering on all embeddings for a specific stream.
///
/// - Loads all embeddings, filters by stream
/// - Computes K = sqrt(n/2) capped by config
/// - Runs K-means with cosine similarity
/// - Stores cluster_id per chunk and centroids in RocksDB
pub fn cluster_stream(
    store: &Arc<RocksDbStore>,
    stream_id: &str,
    config: &AssociatorConfig,
) -> Result<ClusteringResult> {
    info!("Starting clustering for stream {}", stream_id);

    // Load all embeddings
    let all_embeddings = store
        .get_all_embeddings()
        .context("Failed to load embeddings")?;

    // Filter to stream's chunks (cycle/78: also exclude tombstoned chunks
    // — defense-in-depth in case a legacy zombie embedding pre-dating the
    // delete_memory_fully embedding-cleanup fix slipped past hard-purge).
    let mut stream_embeddings: Vec<(String, Vec<f32>)> = Vec::new();
    for (id, emb) in all_embeddings {
        if let Ok(Some(chunk)) = store.get_chunk(&id) {
            if chunk.stream == stream_id && chunk.is_latest && chunk.deleted_at.is_none() {
                stream_embeddings.push((id, emb));
            }
        }
    }

    let n = stream_embeddings.len();
    if n < 3 {
        info!(
            "Too few chunks ({}) for clustering in stream {}",
            n, stream_id
        );
        return Ok(ClusteringResult {
            num_clusters: 0,
            num_chunks: n,
            iterations: 0,
        });
    }

    // Determine K
    let k = if config.k_clusters > 0 {
        config.k_clusters
    } else {
        ((n as f64 / 2.0).sqrt().ceil() as usize)
            .max(2)
            .min(config.max_clusters)
    };
    let k = k.min(n); // Can't have more clusters than chunks

    info!(
        "Clustering {} chunks into {} clusters for stream {}",
        n, k, stream_id
    );

    // Run K-means
    let (assignments, centroids, iterations) = kmeans(
        &stream_embeddings
            .iter()
            .map(|(_, e)| e.as_slice())
            .collect::<Vec<_>>(),
        k,
        config.max_iterations,
    );

    // Store results in RocksDB
    for (i, (chunk_id, _)) in stream_embeddings.iter().enumerate() {
        let cluster_id = assignments[i];
        let key = format!("assoc:cluster:{}", chunk_id);
        store
            .put(key.as_bytes(), cluster_id.to_string().as_bytes())
            .with_context(|| format!("Failed to store cluster assignment for {}", chunk_id))?;
    }

    for (cluster_id, centroid) in centroids.iter().enumerate() {
        let key = format!("assoc:centroid:{}", cluster_id);
        let encoded = bincode::serialize(centroid).context("Failed to serialize centroid")?;
        store
            .put(key.as_bytes(), &encoded)
            .with_context(|| format!("Failed to store centroid {}", cluster_id))?;
    }

    info!(
        "Clustering complete: {} clusters, {} chunks, {} iterations",
        centroids.len(),
        n,
        iterations
    );

    Ok(ClusteringResult {
        num_clusters: centroids.len(),
        num_chunks: n,
        iterations,
    })
}

/// K-means clustering using cosine similarity.
///
/// Returns (assignments, centroids, iterations_used).
fn kmeans(
    embeddings: &[&[f32]],
    k: usize,
    max_iterations: usize,
) -> (Vec<u32>, Vec<Vec<f32>>, usize) {
    let n = embeddings.len();
    let dim = embeddings[0].len();

    // Initialize centroids: pick k evenly spaced embeddings (deterministic)
    let mut centroids: Vec<Vec<f32>> = Vec::with_capacity(k);
    let step = n.max(1) / k.max(1);
    for i in 0..k {
        let idx = (i * step).min(n - 1);
        centroids.push(embeddings[idx].to_vec());
    }

    let mut assignments = vec![0u32; n];
    let mut iterations = 0;

    for iter in 0..max_iterations {
        iterations = iter + 1;
        let mut changed = false;

        // Assignment step: assign each embedding to nearest centroid
        for (i, emb) in embeddings.iter().enumerate() {
            let mut best_cluster = 0u32;
            let mut best_sim = f64::NEG_INFINITY;
            for (c, centroid) in centroids.iter().enumerate() {
                let sim = cosine_similarity(emb, centroid);
                if sim > best_sim {
                    best_sim = sim;
                    best_cluster = c as u32;
                }
            }
            if assignments[i] != best_cluster {
                assignments[i] = best_cluster;
                changed = true;
            }
        }

        if !changed {
            debug!("K-means converged at iteration {}", iter + 1);
            break;
        }

        // Update step: recompute centroids
        let mut new_centroids = vec![vec![0.0f32; dim]; k];
        let mut counts = vec![0usize; k];

        for (i, emb) in embeddings.iter().enumerate() {
            let c = assignments[i] as usize;
            counts[c] += 1;
            for (j, val) in emb.iter().enumerate() {
                new_centroids[c][j] += val;
            }
        }

        for c in 0..k {
            if counts[c] > 0 {
                let count = counts[c] as f32;
                for val in new_centroids[c].iter_mut().take(dim) {
                    *val /= count;
                }
                // Normalize centroid for cosine similarity
                let norm: f32 = new_centroids[c].iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 0.0 {
                    for val in new_centroids[c].iter_mut().take(dim) {
                        *val /= norm;
                    }
                }
            } else {
                // Empty cluster: keep old centroid
                new_centroids[c] = centroids[c].clone();
            }
        }

        centroids = new_centroids;
    }

    (assignments, centroids, iterations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-6);
    }

    #[test]
    fn test_kmeans_basic() {
        // Two clear clusters
        let embeddings: Vec<Vec<f32>> = vec![
            vec![1.0, 0.0, 0.0],
            vec![0.9, 0.1, 0.0],
            vec![0.95, 0.05, 0.0],
            vec![0.0, 1.0, 0.0],
            vec![0.1, 0.9, 0.0],
            vec![0.05, 0.95, 0.0],
        ];
        let refs: Vec<&[f32]> = embeddings.iter().map(|e| e.as_slice()).collect();

        let (assignments, centroids, _iters) = kmeans(&refs, 2, 100);

        // First 3 should be in one cluster, last 3 in another
        assert_eq!(assignments[0], assignments[1]);
        assert_eq!(assignments[1], assignments[2]);
        assert_eq!(assignments[3], assignments[4]);
        assert_eq!(assignments[4], assignments[5]);
        assert_ne!(assignments[0], assignments[3]);
        assert_eq!(centroids.len(), 2);
    }

    #[test]
    fn test_kmeans_single_cluster() {
        let embeddings: Vec<Vec<f32>> = vec![vec![1.0, 0.0], vec![0.9, 0.1]];
        let refs: Vec<&[f32]> = embeddings.iter().map(|e| e.as_slice()).collect();

        let (assignments, centroids, _) = kmeans(&refs, 1, 50);
        assert_eq!(assignments[0], 0);
        assert_eq!(assignments[1], 0);
        assert_eq!(centroids.len(), 1);
    }

    #[test]
    fn test_kmeans_convergence() {
        // Should converge quickly for well-separated data
        let mut embeddings: Vec<Vec<f32>> = Vec::new();
        for _ in 0..50 {
            embeddings.push(vec![1.0, 0.0, 0.0]);
        }
        for _ in 0..50 {
            embeddings.push(vec![0.0, 1.0, 0.0]);
        }
        let refs: Vec<&[f32]> = embeddings.iter().map(|e| e.as_slice()).collect();

        let (_, _, iters) = kmeans(&refs, 2, 100);
        assert!(
            iters <= 10,
            "Should converge quickly, took {} iterations",
            iters
        );
    }
}
