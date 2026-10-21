//! Query-time graph expansion uses only labels and edges with live provenance.
use anyhow::Result;
use std::collections::HashMap;

use super::{EntityNode, GraphStore};

impl GraphStore {
    /// Resolve live query labels before expanding live one- or two-hop evidence.
    pub fn find_live_related_chunks(
        &self,
        names: &[String],
        max_hops: usize,
        stream: &str,
    ) -> Result<Vec<(String, f64)>> {
        let mut scores = HashMap::new();
        for name in names {
            let Some(entity) = self.get_live_entity_by_name(name, stream)? else {
                continue;
            };
            add_chunks(&mut scores, &entity, 1.0);
            if max_hops == 0 {
                continue;
            }
            for (_, neighbor) in self.get_live_neighbors(&entity.id)? {
                add_chunks(&mut scores, &neighbor, 0.5);
                if max_hops < 2 {
                    continue;
                }
                for (_, target) in self.get_live_neighbors(&neighbor.id)? {
                    if target.id != entity.id {
                        add_chunks(&mut scores, &target, 0.25);
                    }
                }
            }
        }
        let mut output: Vec<_> = scores.into_iter().collect();
        output.sort_by(|left, right| right.1.total_cmp(&left.1));
        Ok(output)
    }
}

fn add_chunks(scores: &mut HashMap<String, f64>, entity: &EntityNode, weight: f64) {
    for id in &entity.chunk_ids {
        let score = scores.entry(id.clone()).or_insert(0.0);
        *score = score.max(weight);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::RocksDbConfig,
        storage::{Chunk, RocksDbStore},
    };
    use std::sync::Arc;

    #[test]
    fn live_expansion_requires_live_query_label_even_if_raw_index_survives() {
        let directory = tempfile::tempdir().unwrap();
        let config = RocksDbConfig {
            max_open_files: 100,
            compression: "none".into(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        };
        let store = Arc::new(RocksDbStore::open(directory.path(), &config).unwrap());
        let graph = GraphStore::new(store.clone());
        let source: Chunk = serde_json::from_value(serde_json::json!({
            "id":"support", "content":"Synthetic source for query expansion", "stream":"fixture",
            "level":0,"score":1.0,"timestamp":1000,"consolidated":false,
            "dormant":false,"in_progress":false
        }))
        .unwrap();
        store.store_chunk(&source).unwrap();
        let pii = crate::PiiFilter::new(crate::config::PiiConfig::default()).unwrap();
        graph
            .link_entity(super::super::EntityContribution {
                name: "SyntheticEntity",
                entity_type: "Organization",
                aliases: &["SyntheticAlias".into()],
                stream: "fixture",
                chunk_id: &source.id,
                expected_content: &source.content,
                pii: &pii,
            })
            .unwrap();
        assert_eq!(
            graph
                .find_live_related_chunks(&["SyntheticAlias".into()], 2, "fixture")
                .unwrap(),
            vec![(source.id.clone(), 1.0)]
        );
        store.delete_by_id(&source.id).unwrap();
        assert!(graph
            .get_entity_by_name("SyntheticAlias", "fixture")
            .unwrap()
            .is_some());
        assert!(graph
            .find_live_related_chunks(&["SyntheticAlias".into()], 2, "fixture")
            .unwrap()
            .is_empty());
    }
}
