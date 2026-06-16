//! Temporal co-occurrence associations (ECA-20).
//!
//! Finds chunks stored within a time window of the given chunk,
//! but in a different topic cluster. Pure timestamp + cluster filter,
//! zero embedding computation needed.

use anyhow::Result;
use std::sync::Arc;
use tracing::debug;

use crate::associator::clustering::{cosine_similarity, get_cluster_id};
use crate::storage::{Chunk, RocksDbStore};

/// A temporal co-occurrence candidate.
#[derive(Debug, Clone)]
pub struct TemporalCandidate {
    pub chunk_id: String,
    pub content: String,
    pub score: f64,
    pub time_delta_secs: u64,
    pub cluster_id: u32,
}

/// Find chunks stored within a time window of the anchor chunks,
/// but in different topic clusters.
///
/// # Arguments
/// - `store`: RocksDB store
/// - `anchor_chunk_ids`: IDs of anchor chunks (e.g., search results)
/// - `window_hours`: time window in hours (default 24)
/// - `max_results`: maximum candidates to return
/// - `stream_id`: filter to same stream
pub fn find_temporal_neighbors(
    store: &Arc<RocksDbStore>,
    anchor_chunk_ids: &[String],
    window_hours: u64,
    max_results: usize,
    stream_id: &str,
) -> Result<Vec<TemporalCandidate>> {
    let window_secs = window_hours * 3600;

    // Get anchor chunks and their timestamps + clusters
    let mut anchors: Vec<(u64, u32)> = Vec::new(); // (timestamp, cluster_id)
    let mut anchor_set: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut anchor_clusters: std::collections::HashSet<u32> = std::collections::HashSet::new();

    for id in anchor_chunk_ids {
        anchor_set.insert(id.clone());
        if let Ok(Some(chunk)) = store.get_chunk(id) {
            if let Ok(Some(cluster)) = get_cluster_id(store, id) {
                anchors.push((chunk.timestamp, cluster));
                anchor_clusters.insert(cluster);
            }
        }
    }

    if anchors.is_empty() {
        return Ok(Vec::new());
    }

    // Load anchor embeddings once for cosine similarity filtering
    let mut anchor_embeddings: Vec<Vec<f32>> = Vec::new();
    for id in anchor_chunk_ids {
        if let Ok(Some(emb)) = store.get_embedding(id) {
            anchor_embeddings.push(emb);
        }
    }

    // Scan chunks in the stream, filtering by time window and cluster
    let mut candidates: Vec<TemporalCandidate> = Vec::new();

    for level in 0..=1 {
        let prefix = format!("chunk:L{}:", level);
        for (_key, value) in store.prefix_scan(prefix.as_bytes()) {
            let chunk: Chunk = match store.decode_chunk(&value) {
                Ok(c) => c,
                Err(_) => continue,
            };

            // Filter: same stream, latest, not deleted, not meta/operational
            if !crate::associator::is_associable_in_stream(&chunk, stream_id) {
                continue;
            }
            if anchor_set.contains(&chunk.id) {
                continue;
            }

            // Check cluster: must be in a DIFFERENT cluster than all anchors
            let chunk_cluster = match get_cluster_id(store, &chunk.id)? {
                Some(c) => c,
                None => continue,
            };
            if anchor_clusters.contains(&chunk_cluster) {
                continue;
            }

            // Check time window: chunk must be within ±window_secs of any anchor
            let mut min_delta = u64::MAX;
            for (anchor_ts, _) in &anchors {
                let delta = if chunk.timestamp > *anchor_ts {
                    chunk.timestamp - anchor_ts
                } else {
                    anchor_ts - chunk.timestamp
                };
                min_delta = min_delta.min(delta);
            }

            if min_delta > window_secs {
                continue;
            }

            // Minimum cosine similarity check: filter out topically unrelated chunks
            if !anchor_embeddings.is_empty() {
                if let Ok(Some(candidate_emb)) = store.get_embedding(&chunk.id) {
                    let max_cos = anchor_embeddings
                        .iter()
                        .map(|a| cosine_similarity(a, &candidate_emb))
                        .fold(f64::NEG_INFINITY, f64::max);
                    if max_cos < 0.15 {
                        continue;
                    }
                }
                // If candidate has no embedding, skip the cosine check (allow through)
            }

            // Score by temporal proximity: 1.0 - (delta / window)
            let score = 1.0 - (min_delta as f64 / window_secs as f64);

            candidates.push(TemporalCandidate {
                chunk_id: chunk.id.clone(),
                content: chunk.content.clone(),
                score,
                time_delta_secs: min_delta,
                cluster_id: chunk_cluster,
            });
        }
    }

    // Sort by score descending
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.truncate(max_results);

    debug!(
        "Temporal co-occurrence: found {} candidates within {}h window",
        candidates.len(),
        window_hours
    );

    Ok(candidates)
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_temporal_score() {
        // Score should be 1.0 when delta = 0, 0.0 when delta = window
        let window = 86400u64; // 24h in seconds

        let score_immediate = 1.0 - (0.0 / window as f64);
        assert!((score_immediate - 1.0).abs() < 0.001);

        let score_12h = 1.0 - (43200.0 / window as f64);
        assert!((score_12h - 0.5).abs() < 0.001);

        let score_boundary = 1.0 - (window as f64 / window as f64);
        assert!(score_boundary.abs() < 0.001);
    }
}
