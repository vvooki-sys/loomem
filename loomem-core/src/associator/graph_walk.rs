//! Graph random walk with weak-tie preference (ECA-18).
//!
//! Walks the entity graph preferring edges with fewer shared chunks
//! (weak ties = more novel connections). Returns association candidates
//! from chunks linked to entities at the end of the walk.

use anyhow::Result;
use std::collections::HashSet;
use tracing::debug;

use crate::graph::{Edge, EntityNode, GraphStore};

/// A single step in a graph walk.
#[derive(Debug, Clone)]
pub struct WalkStep {
    pub entity: EntityNode,
    pub edge: Option<Edge>,
}

/// Result of a graph random walk.
#[derive(Debug, Clone)]
pub struct WalkResult {
    pub path: Vec<WalkStep>,
    pub terminal_chunk_ids: Vec<String>,
}

/// Perform a random walk on the entity graph with weak-tie preference.
///
/// At each hop, edges are selected with probability inversely proportional
/// to the number of shared chunks: P(edge) = (1/|chunk_ids|) / Σ(1/|chunk_ids|).
/// This prefers weak ties (fewer shared chunks = higher novelty).
///
/// # Arguments
/// - `graph`: the entity graph store
/// - `seed_entity_id`: starting entity for the walk
/// - `hops`: number of hops to take (default 3)
/// - `count`: number of independent walks to perform
/// - `max_total_hops`: safety cap on total hops across all walks
pub fn random_walk(
    graph: &GraphStore,
    seed_entity_id: &str,
    hops: usize,
    count: usize,
    max_total_hops: usize,
) -> Result<Vec<WalkResult>> {
    let mut results = Vec::new();
    let mut total_hops = 0;

    for walk_idx in 0..count {
        if total_hops >= max_total_hops {
            debug!(
                "Graph walk: hit max_total_hops={}, stopping",
                max_total_hops
            );
            break;
        }

        let mut path = Vec::new();
        let mut visited = HashSet::new();

        // Start from seed
        let seed = graph.get_entity_by_id(seed_entity_id)?;
        let seed = match seed {
            Some(e) => e,
            None => continue,
        };
        visited.insert(seed.id.clone());
        path.push(WalkStep {
            entity: seed.clone(),
            edge: None,
        });

        let mut current = seed;

        for _hop in 0..hops {
            if total_hops >= max_total_hops {
                break;
            }
            total_hops += 1;

            let neighbors = graph.get_neighbors(&current.id)?;
            if neighbors.is_empty() {
                break;
            }

            // Filter out already visited
            let candidates: Vec<&(Edge, EntityNode)> = neighbors
                .iter()
                .filter(|(_, n)| !visited.contains(&n.id))
                .collect();

            if candidates.is_empty() {
                break;
            }

            // Weighted selection: prefer weak ties (fewer shared chunks)
            let next = select_weak_tie(&candidates, walk_idx + total_hops);

            visited.insert(next.1.id.clone());
            path.push(WalkStep {
                entity: next.1.clone(),
                edge: Some(next.0.clone()),
            });
            current = next.1.clone();
        }

        // Collect chunk_ids from terminal entity
        let terminal = path.last().map(|s| &s.entity);
        let terminal_chunk_ids = terminal.map(|e| e.chunk_ids.clone()).unwrap_or_default();

        results.push(WalkResult {
            path,
            terminal_chunk_ids,
        });
    }

    Ok(results)
}

/// Select next edge using weak-tie weighting.
/// P(edge) = (1/|chunk_ids|) / Σ(1/|chunk_ids|)
///
/// Uses a deterministic pseudo-random selection based on `seed` for reproducibility.
fn select_weak_tie<'a>(
    candidates: &[&'a (Edge, EntityNode)],
    seed: usize,
) -> &'a (Edge, EntityNode) {
    if candidates.len() == 1 {
        return candidates[0];
    }

    // Compute inverse weights (weak-tie preference)
    let weights: Vec<f64> = candidates
        .iter()
        .map(|(edge, _)| 1.0 / (edge.chunk_ids.len().max(1) as f64))
        .collect();

    let total: f64 = weights.iter().sum();

    // Deterministic selection using seed
    let threshold = ((seed * 2654435761) % 1000) as f64 / 1000.0 * total;
    let mut cumulative = 0.0;
    for (i, w) in weights.iter().enumerate() {
        cumulative += w;
        if cumulative >= threshold {
            return candidates[i];
        }
    }

    candidates.last().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_weak_tie_prefers_fewer_chunks() {
        // Edge with 1 chunk should be preferred over edge with 10 chunks
        let edge_weak = Edge {
            id: "e1".into(),
            source_entity_id: "a".into(),
            target_entity_id: "b".into(),
            relation_type: "related".into(),
            chunk_ids: vec!["c1".into()], // 1 chunk = weak tie
            stream_id: "test".into(),
            created_at: 0,
            updated_at: 0,
        };
        let entity_b = EntityNode {
            id: "b".into(),
            canonical_name: "B".into(),
            entity_type: "thing".into(),
            aliases: vec![],
            chunk_ids: vec![],
            stream_id: "test".into(),
            created_at: 0,
            updated_at: 0,
        };

        let edge_strong = Edge {
            id: "e2".into(),
            source_entity_id: "a".into(),
            target_entity_id: "c".into(),
            relation_type: "related".into(),
            chunk_ids: (0..10).map(|i| format!("c{}", i)).collect(), // 10 chunks = strong tie
            stream_id: "test".into(),
            created_at: 0,
            updated_at: 0,
        };
        let entity_c = EntityNode {
            id: "c".into(),
            canonical_name: "C".into(),
            entity_type: "thing".into(),
            aliases: vec![],
            chunk_ids: vec![],
            stream_id: "test".into(),
            created_at: 0,
            updated_at: 0,
        };

        let pair_weak = (edge_weak, entity_b);
        let pair_strong = (edge_strong, entity_c);
        let candidates = vec![&pair_weak, &pair_strong];

        // Run multiple selections to verify weak tie is preferred more often
        let mut weak_count = 0;
        for seed in 0..100 {
            let selected = select_weak_tie(&candidates, seed);
            if selected.0.chunk_ids.len() == 1 {
                weak_count += 1;
            }
        }

        // Weak tie (weight 1.0) vs strong tie (weight 0.1) → weak should win ~91% of the time
        assert!(
            weak_count > 70,
            "Weak tie should be selected most of the time, got {}/100",
            weak_count
        );
    }
}
