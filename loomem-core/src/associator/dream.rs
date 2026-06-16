//! Dream mode — offline discovery of latent associations (ECA-23a/23c).
//!
//! During dream cycles, cross-cluster graph walks discover non-obvious
//! connections between chunks. High-scoring discoveries are stored as
//! latent associations and may be promoted to search results later.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{debug, info};

use crate::config::AssociatorConfig;
use crate::graph::GraphStore;
use crate::storage::RocksDbStore;

use super::clustering::{get_all_centroids, get_cluster_id};
use super::graph_walk::random_walk;
use super::serendipity::compute_serendipity;

/// A latent association discovered during dream mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatentAssociation {
    pub id: String,
    pub source_chunk_id: String,
    pub target_chunk_id: String,
    pub target_content: String,
    pub score: f64,
    pub mechanism: String,
    pub discovered_at: u64,
    pub promoted: bool,
    pub promoted_count: u32,
}

/// Report from a dream discovery cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DreamReport {
    pub discoveries: usize,
    pub chunks_explored: usize,
    pub duration_ms: u64,
}

/// Run dream discovery: select random chunks from different clusters,
/// perform cross-cluster graph walks, score with Se, store high-scorers.
pub fn dream_discover(
    store: &Arc<RocksDbStore>,
    graph: &Arc<GraphStore>,
    config: &AssociatorConfig,
    stream_id: &str,
) -> Result<DreamReport> {
    let start = std::time::Instant::now();
    let now = chrono::Utc::now().timestamp() as u64;

    // 1. Get all centroids and pick one chunk per cluster (up to 10)
    let centroids = get_all_centroids(store)?;
    if centroids.is_empty() {
        return Ok(DreamReport {
            discoveries: 0,
            chunks_explored: 0,
            duration_ms: 0,
        });
    }

    // Use deterministic pseudo-random: hash of timestamp to pick chunks
    let seed = now
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);

    // Collect one chunk per cluster
    let mut seed_chunks: Vec<(String, u32)> = Vec::new(); // (chunk_id, cluster_id)
    for (cluster_id, _centroid) in &centroids {
        // Scan for a chunk in this cluster
        let prefix = b"assoc:cluster:";
        let mut candidates: Vec<String> = Vec::new();
        for (key, value) in store.prefix_scan(prefix) {
            let key_str = String::from_utf8_lossy(&key);
            if let Some(chunk_id) = key_str.strip_prefix("assoc:cluster:") {
                let val_str = String::from_utf8_lossy(&value);
                if let Ok(cid) = val_str.parse::<u32>() {
                    if cid == *cluster_id {
                        // Check stream membership
                        if let Ok(Some(chunk)) = store.get_chunk(chunk_id) {
                            if chunk.stream == stream_id
                                && chunk.is_latest
                                && chunk.deleted_at.is_none()
                            {
                                candidates.push(chunk_id.to_string());
                                if candidates.len() >= 5 {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        if !candidates.is_empty() {
            // Pick pseudo-randomly from candidates
            let idx = (seed.wrapping_mul((*cluster_id as u64).wrapping_add(1))) as usize
                % candidates.len();
            seed_chunks.push((candidates[idx].clone(), *cluster_id));
        }
        if seed_chunks.len() >= 10 {
            break;
        }
    }

    if seed_chunks.is_empty() {
        return Ok(DreamReport {
            discoveries: 0,
            chunks_explored: 0,
            duration_ms: start.elapsed().as_millis() as u64,
        });
    }

    debug!(
        "Dream: selected {} seed chunks from {} clusters",
        seed_chunks.len(),
        centroids.len()
    );

    let min_se = config.min_serendipity;
    let mut discoveries = 0usize;
    let mut chunks_explored = 0usize;
    let mut seen_targets: HashSet<String> = HashSet::new();

    // 2. For each seed chunk: graph walk crossing cluster boundaries, score candidates
    for (source_chunk_id, source_cluster) in &seed_chunks {
        // Get source embedding for context
        let source_emb = match store.get_embedding(source_chunk_id)? {
            Some(e) => e,
            None => continue,
        };

        // Find entities linked to this chunk via graph
        let entity_ids: Vec<String> = match graph.get_entities_for_chunk(source_chunk_id) {
            Ok(ids) => ids,
            Err(_) => continue,
        };

        if entity_ids.is_empty() {
            continue;
        }

        // 3-hop walk from each entity, crossing cluster boundaries
        for entity_id in entity_ids.iter().take(3) {
            let walks = match random_walk(graph, entity_id, 3, 2, 20) {
                Ok(w) => w,
                Err(_) => continue,
            };

            for walk in &walks {
                for target_chunk_id in &walk.terminal_chunk_ids {
                    if target_chunk_id == source_chunk_id {
                        continue;
                    }
                    if seen_targets.contains(target_chunk_id) {
                        continue;
                    }
                    seen_targets.insert(target_chunk_id.clone());
                    chunks_explored += 1;

                    // Must be in same stream, associable
                    let target_chunk = match store.get_chunk(target_chunk_id)? {
                        Some(c) => c,
                        None => continue,
                    };
                    if !super::is_associable_in_stream(&target_chunk, stream_id) {
                        continue;
                    }

                    // Must be in a DIFFERENT cluster (cross-cluster discovery)
                    let target_cluster = get_cluster_id(store, target_chunk_id)?.unwrap_or(0);
                    if target_cluster == *source_cluster {
                        continue;
                    }

                    // Score with Se
                    let target_emb = match store.get_embedding(target_chunk_id)? {
                        Some(e) => e,
                        None => continue,
                    };

                    let ctx_refs: Vec<&[f32]> = vec![source_emb.as_slice()];
                    let se = match compute_serendipity(
                        &target_emb,
                        &source_emb,
                        &ctx_refs,
                        target_cluster,
                        *source_cluster,
                        store,
                    ) {
                        Ok(s) => s,
                        Err(_) => continue,
                    };

                    if se.score >= min_se {
                        let latent_id = format!("dream-{}-{}", now, discoveries);
                        let latent = LatentAssociation {
                            id: latent_id.clone(),
                            source_chunk_id: source_chunk_id.clone(),
                            target_chunk_id: target_chunk_id.clone(),
                            target_content: target_chunk.content.clone(),
                            score: se.score,
                            mechanism: "dream_discovery".to_string(),
                            discovered_at: now,
                            promoted: false,
                            promoted_count: 0,
                        };

                        // Store in RocksDB
                        let key = format!("assoc:latent:{}:{}", stream_id, latent_id);
                        let value = serde_json::to_vec(&latent)
                            .context("Failed to serialize latent association")?;
                        store.put(key.as_bytes(), &value)?;

                        discoveries += 1;
                        debug!(
                            "Dream discovery: {} -> {} (Se={:.3})",
                            source_chunk_id, target_chunk_id, se.score
                        );
                    }
                }
            }
        }
    }

    let duration_ms = start.elapsed().as_millis() as u64;

    info!(
        "Dream cycle complete for stream {}: {} discoveries from {} chunks explored in {}ms",
        stream_id, discoveries, chunks_explored, duration_ms
    );

    Ok(DreamReport {
        discoveries,
        chunks_explored,
        duration_ms,
    })
}

/// Get all latent associations for a stream.
pub fn get_latent_associations(
    store: &RocksDbStore,
    stream_id: &str,
    limit: usize,
) -> Result<Vec<LatentAssociation>> {
    let prefix = format!("assoc:latent:{}:", stream_id);
    let mut latents: Vec<LatentAssociation> = Vec::new();

    for (_key, value) in store.prefix_scan(prefix.as_bytes()) {
        if let Ok(latent) = serde_json::from_slice::<LatentAssociation>(&value) {
            latents.push(latent);
        }
        if latents.len() >= limit * 2 {
            break; // scan cap
        }
    }

    // Sort by discovered_at descending
    latents.sort_by_key(|b| std::cmp::Reverse(b.discovered_at));
    latents.truncate(limit);
    Ok(latents)
}

/// Get unpromoted latent associations for a stream.
pub fn get_unpromoted_latent_associations(
    store: &RocksDbStore,
    stream_id: &str,
    limit: usize,
) -> Result<Vec<LatentAssociation>> {
    let prefix = format!("assoc:latent:{}:", stream_id);
    let mut latents: Vec<LatentAssociation> = Vec::new();

    for (_key, value) in store.prefix_scan(prefix.as_bytes()) {
        if let Ok(latent) = serde_json::from_slice::<LatentAssociation>(&value) {
            if !latent.promoted {
                latents.push(latent);
            }
        }
    }

    latents.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    latents.truncate(limit);
    Ok(latents)
}

/// Promote a latent association (increment promoted_count, set promoted=true).
pub fn promote_latent(store: &RocksDbStore, stream_id: &str, latent_id: &str) -> Result<()> {
    let key = format!("assoc:latent:{}:{}", stream_id, latent_id);
    if let Some(value) = store.get(key.as_bytes())? {
        let mut latent: LatentAssociation = serde_json::from_slice(&value)?;
        latent.promoted = true;
        latent.promoted_count += 1;
        let updated = serde_json::to_vec(&latent)?;
        store.put(key.as_bytes(), &updated)?;
    }
    Ok(())
}

/// Count total latent associations for a stream.
pub fn count_latents(store: &RocksDbStore, stream_id: &str) -> usize {
    let prefix = format!("assoc:latent:{}:", stream_id);
    let mut count = 0usize;
    for _ in store.prefix_scan(prefix.as_bytes()) {
        count += 1;
    }
    count
}

// ---- ECA-23c: FIFO eviction ----

/// Evict old latent associations, keeping at most `max_count` per stream (FIFO by discovered_at).
pub fn evict_old_latents(store: &RocksDbStore, stream_id: &str, max_count: usize) -> Result<usize> {
    let prefix = format!("assoc:latent:{}:", stream_id);
    let mut all: Vec<(String, LatentAssociation)> = Vec::new();

    for (key, value) in store.prefix_scan(prefix.as_bytes()) {
        let key_str = String::from_utf8_lossy(&key).to_string();
        if let Ok(latent) = serde_json::from_slice::<LatentAssociation>(&value) {
            all.push((key_str, latent));
        }
    }

    if all.len() <= max_count {
        return Ok(0);
    }

    // Sort by discovered_at ascending (oldest first)
    all.sort_by_key(|(_, l)| l.discovered_at);

    let to_evict = all.len() - max_count;
    let mut evicted = 0usize;

    for (key, _) in all.iter().take(to_evict) {
        if let Err(e) = store.delete(key.as_bytes()) {
            tracing::warn!("Failed to evict latent {}: {}", key, e);
        } else {
            evicted += 1;
        }
    }

    if evicted > 0 {
        info!(
            "Evicted {} old latent associations for stream {} (cap={})",
            evicted, stream_id, max_count
        );
    }

    Ok(evicted)
}

/// Compute dream statistics for a stream.
pub fn dream_stats(store: &RocksDbStore, stream_id: &str) -> DreamStats {
    let prefix = format!("assoc:latent:{}:", stream_id);
    let mut total = 0usize;
    let mut promoted = 0usize;
    let mut total_promoted_count = 0u32;

    for (_key, value) in store.prefix_scan(prefix.as_bytes()) {
        if let Ok(latent) = serde_json::from_slice::<LatentAssociation>(&value) {
            total += 1;
            if latent.promoted {
                promoted += 1;
            }
            total_promoted_count += latent.promoted_count;
        }
    }

    DreamStats {
        total_latent: total,
        promoted_count: promoted,
        total_promotions: total_promoted_count,
        discovery_rate: 0.0, // filled by caller if needed
        promotion_rate: if total > 0 {
            promoted as f64 / total as f64
        } else {
            0.0
        },
    }
}

/// Statistics about dream discoveries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DreamStats {
    pub total_latent: usize,
    pub promoted_count: usize,
    pub total_promotions: u32,
    pub discovery_rate: f64,
    pub promotion_rate: f64,
}
