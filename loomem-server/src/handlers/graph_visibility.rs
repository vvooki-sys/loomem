//! Graph metadata is visible only while authoritative same-stream support is live.
//! Entity aliases remain shared metadata: there is no per-alias source provenance.
use anyhow::Result;
use loomem_core::graph::{Edge, EntityNode};
use serde_json::{json, Value};

use crate::AppState;

pub(crate) struct VisibleGraph {
    entity: EntityNode,
    neighbors: Vec<(Edge, EntityNode)>,
}

/// Resolve and filter before serializing any names, aliases, relations, or IDs.
pub(crate) fn find(state: &AppState, name: &str, stream: &str) -> Result<Option<VisibleGraph>> {
    let Some(entity) = live_entity(state, name, stream)? else {
        return Ok(None);
    };
    let neighbors = live_neighbors(state, &entity)?;
    Ok(Some(VisibleGraph { entity, neighbors }))
}

fn live_entity(state: &AppState, name: &str, stream: &str) -> Result<Option<EntityNode>> {
    Ok(state
        .graph
        .get_entity_by_name(name, stream)?
        .and_then(|entity| supported_entity(state, entity, stream)))
}

fn supported_entity(state: &AppState, mut entity: EntityNode, stream: &str) -> Option<EntityNode> {
    if entity.stream_id != stream {
        return None;
    }
    entity
        .chunk_ids
        .retain(|id| live_support(state, id, stream));
    (!entity.chunk_ids.is_empty()).then_some(entity)
}

fn live_support(state: &AppState, id: &str, stream: &str) -> bool {
    state
        .store
        .get_chunk(id)
        .ok()
        .flatten()
        .is_some_and(|chunk| chunk.deleted_at.is_none() && chunk.stream == stream)
}

fn live_neighbors(state: &AppState, entity: &EntityNode) -> Result<Vec<(Edge, EntityNode)>> {
    Ok(state
        .graph
        .get_neighbors(&entity.id)?
        .into_iter()
        .filter_map(|(edge, target)| supported_neighbor(state, entity, edge, target))
        .collect())
}

fn supported_neighbor(
    state: &AppState,
    entity: &EntityNode,
    mut edge: Edge,
    target: EntityNode,
) -> Option<(Edge, EntityNode)> {
    if edge.stream_id != entity.stream_id || !matching_endpoints(entity, &edge, &target) {
        return None;
    }
    let target = supported_entity(state, target, &entity.stream_id)?;
    edge.chunk_ids
        .retain(|id| entity.chunk_ids.contains(id) && target.chunk_ids.contains(id));
    (!edge.chunk_ids.is_empty()).then_some((edge, target))
}

fn matching_endpoints(entity: &EntityNode, edge: &Edge, target: &EntityNode) -> bool {
    (edge.source_entity_id == entity.id && edge.target_entity_id == target.id)
        || (edge.target_entity_id == entity.id && edge.source_entity_id == target.id)
}

impl VisibleGraph {
    pub(crate) fn json(&self) -> Value {
        let entity = &self.entity;
        let neighbors: Vec<_> = self.neighbors.iter().map(|(edge, target)| json!({
            "entity":target.canonical_name, "entity_type":target.entity_type,
            "relation":edge.relation_type,
            "direction":if edge.source_entity_id == entity.id { "outgoing" } else { "incoming" }
        })).collect();
        json!({"entity":{"id":entity.id,"name":entity.canonical_name,
            "type":entity.entity_type.to_lowercase(),"aliases":entity.aliases,
            "chunk_count":entity.chunk_ids.len()},
            "neighbors":neighbors,"chunk_ids":entity.chunk_ids})
    }

    pub(crate) fn text(&self) -> String {
        let entity = &self.entity;
        let mut text = format!(
            "Entity: {} (type: {})\n",
            entity.canonical_name, entity.entity_type
        );
        if !entity.aliases.is_empty() {
            text.push_str(&format!("Aliases: {}\n", entity.aliases.join(", ")));
        }
        text.push_str(&format!("Linked chunks: {}\n\n", entity.chunk_ids.len()));
        if self.neighbors.is_empty() {
            text.push_str("No connections found.\n");
        } else {
            text.push_str("Connections:\n");
            for (edge, target) in &self.neighbors {
                text.push_str(&format!(
                    "  → {} ({}) [{}]\n",
                    target.canonical_name, edge.relation_type, target.entity_type
                ));
            }
        }
        text
    }
}

#[cfg(test)]
mod tests;
