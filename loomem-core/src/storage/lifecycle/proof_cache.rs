//! Legacy entity/relation cache is source proof only while source bytes/scope match.
use crate::storage::Chunk;
use rocksdb::WriteBatch;

pub(super) fn invalidate_changed_source(
    batch: &mut WriteBatch,
    chunk: &Chunk,
    previous: Option<&Chunk>,
) {
    let Some(old) = previous else {
        return;
    };
    if old.content != chunk.content || old.stream != chunk.stream {
        batch.delete(format!("entity:{}", chunk.id));
        batch.delete(format!("rel:{}", chunk.id));
        batch.delete(format!("llm_entities:{}", chunk.id));
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        config::RocksDbConfig,
        storage::{Chunk, RocksDbStore},
    };
    #[test]
    fn source_content_or_scope_change_invalidates_legacy_proof_but_score_and_first_write_do_not() {
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
        let mut chunk:Chunk=serde_json::from_value(serde_json::json!({"id":"legacy:proof","content":"first","stream":"test","level":0,"score":1,"timestamp":1,"consolidated":false,"dormant":false,"in_progress":false})).unwrap();
        store
            .store_entities(
                &chunk.id,
                &chunk.stream,
                &[("old_entity".into(), "synthetic".into())],
            )
            .unwrap();
        store
            .store_relations(
                &chunk.id,
                &chunk.stream,
                &[("old_entity".into(), "uses".into(), "target".into())],
            )
            .unwrap();
        store.put(b"llm_entities:legacy:proof", b"1").unwrap();
        store.store_chunk(&chunk).unwrap();
        assert!(store.get(b"entity:legacy:proof").unwrap().is_some());
        assert!(store.get(b"rel:legacy:proof").unwrap().is_some());
        chunk.score = 0.5;
        store.store_chunk(&chunk).unwrap();
        assert!(store.get(b"entity:legacy:proof").unwrap().is_some());
        assert!(store.get(b"rel:legacy:proof").unwrap().is_some());
        assert!(store.get(b"llm_entities:legacy:proof").unwrap().is_some());
        chunk.content = "edited".into();
        store.store_chunk(&chunk).unwrap();
        assert!(store.get(b"llm_entities:legacy:proof").unwrap().is_none());
        assert!(store.get(b"entity:legacy:proof").unwrap().is_none());
        assert!(store.get(b"rel:legacy:proof").unwrap().is_none());
        assert!(store
            .get_entities(&chunk.id, &chunk.stream)
            .unwrap()
            .is_empty());
        assert!(store
            .get_relations(&chunk.id, &chunk.stream)
            .unwrap()
            .is_empty());
        store
            .store_entities(
                &chunk.id,
                &chunk.stream,
                &[("fresh_entity".into(), "synthetic".into())],
            )
            .unwrap();
        store
            .store_relations(
                &chunk.id,
                &chunk.stream,
                &[("fresh_entity".into(), "uses".into(), "target".into())],
            )
            .unwrap();
        chunk.stream = "other".into();
        store.store_chunk(&chunk).unwrap();
        assert!(store.get(b"llm_entities:legacy:proof").unwrap().is_none());
        assert!(store.get(b"entity:legacy:proof").unwrap().is_none());
        assert!(store.get(b"rel:legacy:proof").unwrap().is_none());
    }
}
