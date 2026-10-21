use super::*;
use crate::{
    config::RocksDbConfig,
    source_tag::SourceTag,
    storage::{Chunk, ProvenanceRole},
    tantivy_index::{AgentSearchParams, TantivyConfig},
};

fn index(path: &std::path::Path) -> TantivyIndex {
    TantivyIndex::open(
        path,
        &TantivyConfig {
            enabled: true,
            heap_size_mb: 16,
            drift_warn_pct: 5.0,
            auto_rebuild_on_drift: false,
        },
    )
    .unwrap()
}
fn chunk(id: &str) -> Chunk {
    serde_json::from_value(serde_json::json!({"id":id,"content":"synthetic temporal","stream":"test","level":0,"score":1.0,"timestamp":1700000000,"consolidated":false,"dormant":false,"in_progress":false})).unwrap()
}
fn source() -> Chunk {
    let mut c = chunk(&uuid::Uuid::new_v4().to_string());
    c.persistent = true;
    c.consolidated = true;
    c.provenance_role = ProvenanceRole::Evidence;
    c.trust_level = Some("a2".into());
    c.source = Some(SourceTag::from_agent("mcp-ingest-source"));
    c.metadata = Some(serde_json::json!({"_loomem_ingest":{"version":1,"kind":"source"}}));
    c
}
fn fact(parent: &Chunk, date: Option<&str>) -> Chunk {
    let mut c = chunk(&uuid::Uuid::new_v4().to_string());
    c.source = Some(SourceTag::from_agent("mcp-ingest"));
    c.trust_level = Some("a2".into());
    c.metadata = Some(serde_json::json!({"_loomem_ingest":{"version":1,"kind":"fact"}}));
    c.source_ids = Some(vec![parent.id.clone()]);
    c.extraction_meta=Some(serde_json::from_value(serde_json::json!({"fact_type":"fact","subject":null,"event_date":date,"event_date_context":null,"supersedes":null,"superseded_by":null,"confidence":1.0,"extracted_from":parent.id,"extraction_model":null})).unwrap());
    c
}
fn fragment(parent: &Chunk) -> Chunk {
    let mut template = chunk("template");
    template.consolidated = true;
    template.source = Some(SourceTag::from_agent("mcp-ingest"));
    template.trust_level = Some("a2".into());
    template.metadata = Some(serde_json::json!({"_loomem_ingest":{"version":1,"kind":"fragment"}}));
    crate::fragments::fragment_chunk(
        &crate::fragments::SourceFragment {
            id: crate::fragments::fragment_id(&parent.id, 0),
            source_id: parent.id.clone(),
            index: 0,
            byte_start: 0,
            byte_end: parent.content.len(),
            turn_index: 0,
            role: Some("user".into()),
            conversation_date: Some("2024-03-20".into()),
            text: parent.content.clone(),
        },
        &template,
    )
}
fn doc(c: &Chunk) -> TextDocument {
    TextDocument {
        id: c.id.clone(),
        content: c.content.clone(),
        user_id: "default".into(),
        app_id: "default".into(),
        level: c.level,
        timestamp: i64::try_from(c.timestamp).unwrap(),
        stream: c.stream.clone(),
        entities: None,
        relations: None,
        event_date: None,
        source_agent: c.source.as_ref().map(|t| t.agent.clone()),
    }
}
fn ids(index: &TantivyIndex, start: i64, end: i64) -> Vec<String> {
    index
        .search_with_date_range("", start, end, Some("test"), 20)
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect()
}
#[test]
fn nullable_events_known_pre_epoch_and_legacy_all_date_routes() {
    let temp = tempfile::tempdir().unwrap();
    let mut idx = index(temp.path());
    let root = source();
    let unknown = fact(&root, None);
    let known = fact(&root, Some("1956-06-15"));
    let legacy = chunk("legacy");
    for c in [&root, &unknown, &known, &legacy] {
        idx.index_chunk_document(doc(c), c).unwrap();
    }
    idx.commit().unwrap();
    assert_eq!(ids(&idx, 1699999999, 1700000001), vec![legacy.id.clone()]);
    let ts = event_timestamp_for_chunk(&known).unwrap();
    assert!(ts < 0);
    assert_eq!(ids(&idx, ts, ts), vec![known.id.clone()]);
    let results = idx
        .search_with_agent(AgentSearchParams {
            query_text: "",
            stream: Some("test"),
            entity: None,
            date_range: Some((1699999999, 1700000001)),
            source_agent: Some("mcp-ingest"),
            exclude_source_agents: None,
            limit: 20,
        })
        .unwrap();
    assert!(results.is_empty());
    let hits = idx
        .search_with_agent(AgentSearchParams {
            query_text: "temporal",
            stream: Some("test"),
            entity: None,
            date_range: Some((ts, ts)),
            source_agent: Some("mcp-ingest"),
            exclude_source_agents: None,
            limit: 20,
        })
        .unwrap();
    assert_eq!(hits[0].id, known.id);
    assert_eq!(idx.search("temporal", 20).unwrap().len(), 4);
    let mut invalid = fact(&root, Some("2026-02-30"));
    assert_eq!(event_timestamp_for_chunk(&invalid), None);
    invalid.extraction_meta.as_mut().unwrap().event_date = Some("2026-1-01".into());
    assert_eq!(event_timestamp_for_chunk(&invalid), None);
}
/// A fragment's event date is unknown like its source's: the date-range lane
/// must not match it by storage timestamp, while plain search still finds it.
#[test]
fn fragment_event_date_is_never_the_storage_timestamp() {
    let temp = tempfile::tempdir().unwrap();
    let mut idx = index(temp.path());
    let root = source();
    let piece = fragment(&root);
    assert!(crate::storage::is_ingest_fragment(&piece));
    assert_eq!(event_timestamp_for_chunk(&piece), None);
    let legacy = chunk("legacy");
    for c in [&root, &piece, &legacy] {
        idx.index_chunk_document(doc(c), c).unwrap();
    }
    idx.commit().unwrap();
    assert_eq!(ids(&idx, 1699999999, 1700000001), vec![legacy.id.clone()]);
    assert_eq!(idx.search("temporal", 20).unwrap().len(), 3);
}
#[test]
fn old_index_repair_is_non_destructive_idempotent_and_survives_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let store = RocksDbStore::open(
        temp.path().join("db"),
        &RocksDbConfig {
            max_open_files: 100,
            compression: "lz4".into(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        },
    )
    .unwrap();
    let root = source();
    let unknown = fact(&root, None);
    let known = fact(&root, Some("2026-06-15"));
    let legacy = chunk("legacy");
    for c in [&root, &unknown, &known, &legacy] {
        store.store_chunk(c).unwrap();
    }
    let path = temp.path().join("index");
    let mut idx = index(&path);
    for c in [&root, &unknown, &known, &legacy] {
        idx.index_document(doc(c)).unwrap();
    }
    idx.commit().unwrap();
    assert_eq!(ids(&idx, 1699999999, 1700000001).len(), 4);
    drop(idx);
    let mut idx = index(&path);
    assert_eq!(idx.repair_ingest_event_dates(&store).unwrap(), 3);
    assert_eq!(idx.repair_ingest_event_dates(&store).unwrap(), 3);
    assert_eq!(idx.count().unwrap(), 4);
    assert_eq!(ids(&idx, 1699999999, 1700000001), vec![legacy.id.clone()]);
    let ts = event_timestamp_for_chunk(&known).unwrap();
    assert_eq!(ids(&idx, ts, ts), vec![known.id]);
    drop(idx);
    let idx = index(&path);
    assert_eq!(ids(&idx, 1699999999, 1700000001), vec![legacy.id]);
}
#[test]
fn rebuild_and_repair_do_not_resurrect_tombstones_and_repair_decode_is_strict() {
    let temp = tempfile::tempdir().unwrap();
    let store = RocksDbStore::open(
        temp.path().join("db"),
        &RocksDbConfig {
            max_open_files: 100,
            compression: "lz4".into(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        },
    )
    .unwrap();
    let root = source();
    let unknown = fact(&root, None);
    for c in [&root, &unknown] {
        store.store_chunk(c).unwrap();
    }
    let mut idx = index(&temp.path().join("index"));
    idx.rebuild_from_rocksdb(&store).unwrap();
    assert!(ids(&idx, 0, i64::MAX).is_empty());
    assert_eq!(idx.count().unwrap(), 2);
    store.tombstone_ingest_source(&root.id, 1700000001).unwrap();
    idx.repair_ingest_event_dates(&store).unwrap();
    assert_eq!(idx.count().unwrap(), 0);
    store.put(b"chunk:L0:corrupt", b"bad").unwrap();
    assert!(idx.repair_ingest_event_dates(&store).is_err());
}
#[test]
fn identity_binding_prevents_wrong_chunk_policy_and_upserts_preserve_unknown() {
    let temp = tempfile::tempdir().unwrap();
    let mut idx = index(temp.path());
    let root = source();
    let mut d = doc(&root);
    d.id = "mismatch".into();
    assert!(idx.upsert_chunk_document(d, &root).is_err());
    idx.index_document(doc(&root)).unwrap();
    idx.commit().unwrap();
    assert_eq!(ids(&idx, 0, i64::MAX).len(), 1);
    idx.upsert_chunk_document(doc(&root), &root).unwrap();
    idx.commit().unwrap();
    assert!(ids(&idx, 0, i64::MAX).is_empty());
    assert_eq!(idx.count().unwrap(), 1);
}

#[tokio::test]
async fn shared_persist_and_intent_replay_keep_unknown_event_nullable() {
    use crate::intent_log::{recover, IntentLog, IntentLogConfig, OpType};
    use crate::storage::{persist_chunk_with_index, PersistChunkArgs};
    let temp = tempfile::tempdir().unwrap();
    let store = RocksDbStore::open(
        temp.path().join("db"),
        &RocksDbConfig {
            max_open_files: 100,
            compression: "lz4".into(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        },
    )
    .unwrap();
    let index = tokio::sync::Mutex::new(index(&temp.path().join("index")));
    let root = source();
    let unknown = fact(&root, None);
    for c in [&root, &unknown] {
        persist_chunk_with_index(
            &store,
            &index,
            PersistChunkArgs {
                chunk: c,
                text_doc: doc(c),
                intent_log: None,
                op: OpType::Store,
            },
        )
        .await
        .unwrap();
    }
    let mut index = index.into_inner();
    assert!(ids(&index, 0, i64::MAX).is_empty());
    // Emulate stale pre-fix entries, then execute the real recovery writer.
    for c in [&root, &unknown] {
        index.upsert_document(doc(c)).unwrap();
    }
    index.commit().unwrap();
    assert_eq!(ids(&index, 0, i64::MAX).len(), 2);
    let mut log = IntentLog::open(temp.path(), &IntentLogConfig::default()).unwrap();
    for c in [&root, &unknown] {
        log.append_pending(OpType::Store, &c.id).unwrap();
    }
    assert_eq!(recover(&mut log, &store, &mut index).unwrap().replayed, 2);
    assert!(ids(&index, 0, i64::MAX).is_empty());
    assert_eq!(index.count().unwrap(), 2);
}
