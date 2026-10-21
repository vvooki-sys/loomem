//! Physical source deletion removes source-bound graph sidecars in the same batch.
use crate::storage::RocksDbStore;
use anyhow::{Context, Result};
use rocksdb::WriteBatch;
use std::collections::BTreeSet;

impl RocksDbStore {
    pub(super) fn append_graph_sidecar_deletes(
        &self,
        batch: &mut WriteBatch,
        chunk_ids: &BTreeSet<String>,
    ) -> Result<()> {
        for prefix in ["graph:labels:", "graph:edge_labels:"] {
            for row in self.db.prefix_iterator(prefix.as_bytes()) {
                let (key, _) = row.context("scan graph sidecars for physical deletion")?;
                if !key.starts_with(prefix.as_bytes()) {
                    break;
                }
                let key_str =
                    std::str::from_utf8(&key).context("invalid graph sidecar key encoding")?;
                if paired_chunk_id(key_str, prefix).is_some_and(|id| chunk_ids.contains(id)) {
                    batch.delete(key);
                }
            }
        }
        Ok(())
    }
}

fn paired_chunk_id<'a>(key: &'a str, prefix: &str) -> Option<&'a str> {
    key.strip_prefix(prefix)?
        .split_once(':')
        .map(|(_, chunk)| chunk)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RocksDbConfig;

    fn setup() -> (tempfile::TempDir, RocksDbStore) {
        let temp = tempfile::tempdir().unwrap();
        let store = RocksDbStore::open(
            temp.path(),
            &RocksDbConfig {
                max_open_files: 100,
                compression: "none".into(),
                write_buffer_size: 4 * 1024 * 1024,
                max_write_buffer_number: 2,
            },
        )
        .unwrap();
        (temp, store)
    }
    fn chunk(id: &str, stream: &str) -> crate::storage::Chunk {
        serde_json::from_value(serde_json::json!({"id":id,"content":"synthetic","stream":stream,"level":0,"score":1,"timestamp":1,"consolidated":false,"dormant":false,"in_progress":false})).unwrap()
    }
    fn seed(store: &RocksDbStore, id: &str) -> Vec<String> {
        ["graph:labels:", "graph:edge_labels:"]
            .into_iter()
            .map(|prefix| {
                let key = format!("{prefix}{}:{id}", uuid::Uuid::new_v4());
                store
                    .put(key.as_bytes(), b"private sidecar sentinel")
                    .unwrap();
                key
            })
            .collect()
    }
    #[test]
    fn hard_delete_removes_only_exact_complete_paired_chunk_id() {
        let (_temp, store) = setup();
        let target = chunk("legacy:target:id", "test");
        let other = chunk("id", "test");
        store.store_chunk(&target).unwrap();
        store.store_chunk(&other).unwrap();
        store
            .put(format!("llm_entities:{}", target.id).as_bytes(), b"1")
            .unwrap();
        let target_keys = seed(&store, &target.id);
        let keep_keys = seed(&store, &other.id);
        assert!(store.hard_delete_by_id(&target.id).unwrap());
        for key in target_keys {
            assert!(store.get(key.as_bytes()).unwrap().is_none());
        }
        for key in keep_keys {
            assert!(store.get(key.as_bytes()).unwrap().is_some());
        }
        assert!(store
            .get(format!("llm_entities:{}", target.id).as_bytes())
            .unwrap()
            .is_none());
        assert!(!store.hard_delete_by_id(&target.id).unwrap());
    }
    #[test]
    fn stream_purge_removes_sidecars_in_primary_batch_and_dry_run_retains() {
        let (_temp, store) = setup();
        let target = chunk("legacy:purge:id", "purge");
        let other = chunk("other:id", "keep");
        store.store_chunk(&target).unwrap();
        store.store_chunk(&other).unwrap();
        let second = chunk("legacy:other:id", "purge");
        store.store_chunk(&second).unwrap();
        store
            .put(
                format!("llm_entities:{}", target.id).as_bytes(),
                b"old raw response",
            )
            .unwrap();
        let mut target_keys = seed(&store, &target.id);
        target_keys.extend(seed(&store, &second.id));
        let keep_keys = seed(&store, &other.id);
        assert_eq!(store.purge_namespace("purge", true).unwrap().len(), 2);
        for key in &target_keys {
            assert!(store.get(key.as_bytes()).unwrap().is_some());
        }
        assert!(store
            .get(format!("llm_entities:{}", target.id).as_bytes())
            .unwrap()
            .is_some());
        store.purge_namespace("purge", false).unwrap();
        assert!(store
            .get(format!("llm_entities:{}", target.id).as_bytes())
            .unwrap()
            .is_none());
        for key in target_keys {
            assert!(store.get(key.as_bytes()).unwrap().is_none());
        }
        for key in keep_keys {
            assert!(store.get(key.as_bytes()).unwrap().is_some());
        }
    }
}
