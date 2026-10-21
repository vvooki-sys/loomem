//! Old extraction completion payloads are disposable hints, never source evidence.
use crate::storage::RocksDbStore;
use anyhow::{Context, Result};
use rocksdb::WriteBatch;

impl RocksDbStore {
    /// Clear legacy model-response markers and physically orphaned completion hints.
    /// Static hints for existing chunks (including tombstones) remain compatible.
    pub fn cleanup_legacy_entity_markers(&self) -> Result<usize> {
        let _guard = self.lock_lifecycle()?;
        let mut batch = WriteBatch::default();
        let mut removed = 0;
        for row in self.db.prefix_iterator(b"llm_entities:") {
            let (key, value) = row.context("scan extraction completion hints")?;
            if !key.starts_with(b"llm_entities:") {
                break;
            }
            let id = std::str::from_utf8(&key[b"llm_entities:".len()..])
                .context("invalid extraction completion hint ID")?;
            if value.as_ref() != b"1" || self.get_chunk(id)?.is_none() {
                batch.delete(key);
                removed += 1;
            }
        }
        self.db
            .write(batch)
            .context("clear legacy extraction completion hints")?;
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        config::RocksDbConfig,
        storage::{Chunk, RocksDbStore},
    };
    #[test]
    fn cleanup_is_idempotent_preserves_static_live_and_tombstone_hints() {
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
        for id in ["live", "dead", "raw"] {
            let mut chunk: Chunk = serde_json::from_value(serde_json::json!({"id":id,"content":"synthetic","stream":"test","level":0,"score":1,"timestamp":1,"consolidated":false,"dormant":false,"in_progress":false})).unwrap();
            if id == "dead" {
                chunk.deleted_at = Some(2);
            }
            store.store_chunk(&chunk).unwrap();
            store
                .put(
                    format!("llm_entities:{id}").as_bytes(),
                    if id == "raw" {
                        b"raw model sentinel"
                    } else {
                        b"1"
                    },
                )
                .unwrap();
        }
        store.put(b"llm_entities:orphan:with:colon", b"1").unwrap();
        assert_eq!(store.cleanup_legacy_entity_markers().unwrap(), 2);
        assert_eq!(store.cleanup_legacy_entity_markers().unwrap(), 0);
        for id in ["live", "dead"] {
            assert_eq!(
                store.get(format!("llm_entities:{id}").as_bytes()).unwrap(),
                Some(b"1".to_vec())
            );
        }
        assert!(store.get(b"llm_entities:raw").unwrap().is_none());
        assert!(store
            .get(b"llm_entities:orphan:with:colon")
            .unwrap()
            .is_none());
    }
}
