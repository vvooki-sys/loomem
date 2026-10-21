//! Existing raw link helpers retained for legacy writers and graph maintenance.
use crate::graph::{now_secs, GraphStore};
use anyhow::Result;

impl GraphStore {
    /// Add a chunk reference to an entity node.
    pub fn add_chunk_to_entity(&self, entity_id: &str, chunk_id: &str) -> Result<()> {
        if let Some(mut entity) = self.get_entity_by_id(entity_id)? {
            if !entity.chunk_ids.contains(&chunk_id.to_string()) {
                entity.chunk_ids.push(chunk_id.to_string());
                entity.updated_at = now_secs();
                self.store_entity(&entity)?;

                // Update reverse index: chunk -> entities
                self.add_to_chunk_index(chunk_id, entity_id)?;
            }
        }
        Ok(())
    }

    /// Add a chunk reference to an edge.
    pub fn add_chunk_to_edge(&self, edge_id: &str, chunk_id: &str) -> Result<()> {
        if let Some(mut edge) = self.get_edge_by_id(edge_id)? {
            if !edge.chunk_ids.contains(&chunk_id.to_string()) {
                edge.chunk_ids.push(chunk_id.to_string());
                edge.updated_at = now_secs();
                self.store_edge(&edge)?;
            }
        }
        Ok(())
    }
}
