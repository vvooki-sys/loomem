use super::*;
use crate::config::{RocksDbConfig, TantivyConfig};
use crate::storage::{Chunk, RocksDbStore};
use crate::tantivy_index::TantivyIndex;

fn make_chunk(id: &str, stream: &str, content: &str) -> Chunk {
    Chunk {
        id: id.to_string(),
        content: content.to_string(),
        stream: stream.to_string(),
        level: 0,
        score: 0.5,
        timestamp: 1_700_000_000,
        consolidated: false,
        dormant: false,
        in_progress: false,
        prompt_version: None,
        source_ids: None,
        last_decay: None,
        metadata: None,
        importance: None,
        persistent: false,
        last_implicit_boost: None,
        access_count: 0,
        source: None,
        created_by: None,
        updated_at: None,
        valid_from: None,
        valid_until: None,
        is_latest: true,
        superseded_by: None,
        supersedes_id: None,
        root_memory_id: None,
        version: 1,
        memory_type: None,
        extraction_meta: None,
        deleted_at: None,
        trust_level: None,
        ingester_user_id: None,
        alpha: 1.0,
        beta: 1.0,
        harmful_count: 0,
        n_ratings: 0,
        last_rated_at: None,
        provenance_role: crate::storage::ProvenanceRole::Claim,
    }
}

fn backends(path: &Path) -> (RocksDbStore, TantivyIndex) {
    let store = RocksDbStore::open(
        path.join("rocks"),
        &RocksDbConfig {
            max_open_files: 100,
            compression: "lz4".into(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        },
    )
    .unwrap();
    let index = open_index(path);
    (store, index)
}

fn open_index(path: &Path) -> TantivyIndex {
    TantivyIndex::open(
        path.join("index"),
        &TantivyConfig {
            enabled: true,
            heap_size_mb: 50,
            drift_warn_pct: 5.0,
            auto_rebuild_on_drift: false,
        },
    )
    .unwrap()
}

fn index_chunk(index: &mut TantivyIndex, chunk: &Chunk) {
    index
        .upsert_document(crate::tantivy_index::TextDocument {
            id: chunk.id.clone(),
            content: chunk.content.clone(),
            stream: chunk.stream.clone(),
            user_id: String::new(),
            app_id: String::new(),
            level: 0,
            timestamp: 1_700_000_000,
            entities: None,
            relations: None,
            event_date: None,
            source_agent: None,
        })
        .unwrap();
    index.commit().unwrap();
}

#[test]
fn failed_commit_leaves_all_intents_pending_and_retry_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, mut index) = backends(tmp.path());
    let mut log = IntentLog::open(tmp.path(), &IntentLogConfig::default()).unwrap();
    for (id, op) in [("first", OpType::Store), ("second", OpType::Consolidate)] {
        store
            .store_chunk(&make_chunk(id, "alpha", "needleword recovery"))
            .unwrap();
        log.append_pending(op, id).unwrap();
    }
    let error = recovery::recover_with_failed_commit(&mut log, &store, &mut index).unwrap_err();
    assert!(error.to_string().contains("intents remain pending"));
    assert_eq!(log.scan_pending().unwrap().len(), 2);
    drop(index);
    drop(log);
    let mut index = open_index(tmp.path());
    let mut log = IntentLog::open(tmp.path(), &IntentLogConfig::default()).unwrap();
    assert!(index.search("needleword", 10).unwrap().is_empty());
    assert_eq!(recover(&mut log, &store, &mut index).unwrap().replayed, 2);
    assert!(log.scan_pending().unwrap().is_empty());
    assert_eq!(index.search("needleword", 10).unwrap().len(), 2);
    assert_eq!(recover(&mut log, &store, &mut index).unwrap().replayed, 0);
    drop(index);
    let index = open_index(tmp.path());
    assert_eq!(index.search("needleword", 10).unwrap().len(), 2);
}

#[test]
fn absent_and_tombstoned_sources_remove_stale_index_documents() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, mut index) = backends(tmp.path());
    let mut log = IntentLog::open(tmp.path(), &IntentLogConfig::default()).unwrap();
    for (id, op, deleted_at) in [
        ("missing", OpType::Store, None),
        ("tombstone", OpType::Consolidate, Some(1_700_000_001)),
    ] {
        let mut chunk = make_chunk(id, "alpha", "needleword stale");
        index_chunk(&mut index, &chunk);
        chunk.deleted_at = deleted_at;
        if deleted_at.is_some() {
            store.store_chunk(&chunk).unwrap();
        }
        log.append_pending(op, id).unwrap();
    }
    assert_eq!(recover(&mut log, &store, &mut index).unwrap().skipped, 2);
    assert!(index.search("needleword", 10).unwrap().is_empty());
}

#[test]
fn delete_followed_by_old_store_cannot_resurrect_tombstone() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, mut index) = backends(tmp.path());
    let mut log = IntentLog::open(tmp.path(), &IntentLogConfig::default()).unwrap();
    let chunk = make_chunk("gone", "alpha", "needleword deletion");
    store.store_chunk(&chunk).unwrap();
    index_chunk(&mut index, &chunk);
    log.append_pending(OpType::Delete, &chunk.id).unwrap();
    log.append_pending(OpType::Store, &chunk.id).unwrap();
    recover(&mut log, &store, &mut index).unwrap();
    assert!(store
        .get_chunk(&chunk.id)
        .unwrap()
        .unwrap()
        .deleted_at
        .is_some());
    assert!(index.search("needleword", 10).unwrap().is_empty());
}

#[test]
fn purge_recovers_after_primary_rows_already_disappeared() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, mut index) = backends(tmp.path());
    let mut log = IntentLog::open(tmp.path(), &IntentLogConfig::default()).unwrap();
    for (id, stream) in [("gone", "alpha"), ("kept", "alpha-other")] {
        let chunk = make_chunk(id, stream, "needleword purge");
        store.store_chunk(&chunk).unwrap();
        index_chunk(&mut index, &chunk);
    }
    log.append_pending(OpType::Purge, "alpha").unwrap();
    store.purge_namespace("alpha", false).unwrap();
    assert!(store.get_chunk("gone").unwrap().is_none());
    recover(&mut log, &store, &mut index).unwrap();
    let hits = index.search("needleword", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "kept");
    assert!(log.scan_pending().unwrap().is_empty());
}

#[test]
fn restart_after_rotation_preserves_unique_sequence_and_old_pending() {
    let tmp = tempfile::tempdir().unwrap();
    let config = IntentLogConfig {
        max_size_mb: 0,
        ..IntentLogConfig::default()
    };
    let mut log = IntentLog::open(tmp.path(), &config).unwrap();
    let first = log.append_pending(OpType::Store, "old").unwrap();
    drop(log);
    let mut log = IntentLog::open(tmp.path(), &config).unwrap();
    let second = log.append_pending(OpType::Store, "new").unwrap();
    assert!(second > first);
    log.mark_committed(second, OpType::Store, "new").unwrap();
    let pending = log.scan_pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].seq, first);
}

#[test]
fn pending_replay_order_is_numeric_and_commits_survive_archive_order() {
    let tmp = tempfile::tempdir().unwrap();
    let mut log = IntentLog::open(tmp.path(), &IntentLogConfig::default()).unwrap();
    for id in 0..20 {
        log.append_pending(OpType::Store, &id.to_string()).unwrap();
    }
    let seqs: Vec<_> = log.scan_pending().unwrap().iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=20).collect::<Vec<_>>());
    // A clock rollback can put a later commit in an earlier-sorting archive.
    let marker = IntentEntry {
        seq: 1,
        ts: 1,
        op: OpType::Store,
        id: "0".into(),
        status: OpStatus::Committed,
    };
    fs::write(
        log.dir.join("wal_1_0.log"),
        format!("{}\n", serde_json::to_string(&marker).unwrap()),
    )
    .unwrap();
    assert_eq!(log.scan_pending().unwrap().len(), 19);
}

#[test]
fn unreadable_archive_prevents_reusing_unknown_sequences() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("wal/wal_1_0.log")).unwrap();
    assert!(IntentLog::open(tmp.path(), &IntentLogConfig::default()).is_err());
}

#[test]
fn crash_after_index_commit_before_markers_replays_without_duplicates() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, mut index) = backends(tmp.path());
    let mut log = IntentLog::open(tmp.path(), &IntentLogConfig::default()).unwrap();
    store
        .store_chunk(&make_chunk("saved", "alpha", "needleword committed"))
        .unwrap();
    log.append_pending(OpType::Store, "saved").unwrap();
    // Stop recovery at its final commit boundary, then persist only the index,
    // reproducing a crash before the subsequent WAL marker append.
    assert!(recovery::recover_with_failed_commit(&mut log, &store, &mut index).is_err());
    index.commit().unwrap();
    drop(index);
    drop(log);
    let mut index = open_index(tmp.path());
    let mut log = IntentLog::open(tmp.path(), &IntentLogConfig::default()).unwrap();
    assert_eq!(log.scan_pending().unwrap().len(), 1);
    assert_eq!(index.search("needleword", 10).unwrap().len(), 1);
    recover(&mut log, &store, &mut index).unwrap();
    assert_eq!(index.search("needleword", 10).unwrap().len(), 1);
    assert!(log.scan_pending().unwrap().is_empty());
}
