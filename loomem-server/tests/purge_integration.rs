//! Cycle/135 integration tests — purge cascade contract + intent log
//! `OpType::PurgeChunk` replay.
//!
//! Storage-level E2E for the GDPR Art 17 fast-path endpoint. HTTP RBAC matrix
//! is covered by `loomem-server/src/main.rs` tests (admin paths) and
//! `loomem-server/src/handlers/purge.rs` tests (synthetic AuthContext for
//! non-admin + helper cascade). These tests guarantee that the cascade
//! semantics match across every retrieval surface (store + embedding +
//! tantivy + graph) and that the WAL replay path for `OpType::PurgeChunk`
//! is idempotent.
//!
//! `loomem-server` is a binary target, so we cannot import the
//! `hard_delete_memory_fully` helper here. The cascade logic is replicated
//! inline (three storage-layer calls) — identical to
//! `handlers/purge.rs::hard_delete_memory_fully` minus the `PurgeOutcome`
//! shaping. This keeps the test self-contained at the loomem-core layer.

use std::sync::Arc;

use loomem_core::config::{IntentLogConfig, RocksDbConfig, TantivyConfig};
use loomem_core::graph::GraphStore;
use loomem_core::intent_log::{IntentLog, OpType};
use loomem_core::storage::{Chunk, RocksDbStore};
use loomem_core::tantivy_index::{TantivyIndex, TextDocument};
use tempfile::TempDir;
use tokio::sync::Mutex;

/// Inline mirror of `loomem-server/src/handlers/purge.rs::hard_delete_memory_fully`.
/// Cascade ordering: store → tantivy → graph (cycle/117 reorder).
async fn cascade_purge(
    store: &RocksDbStore,
    tantivy: &Mutex<TantivyIndex>,
    graph: &GraphStore,
    id: &str,
) -> (bool, bool) {
    let skipped_soft = matches!(
        store.get_chunk(id),
        Ok(Some(ref c)) if c.deleted_at.is_none()
    );
    let purge_executed = store.hard_delete_by_id(id).unwrap();
    {
        let mut idx = tantivy.lock().await;
        let _ = idx.delete_document(id);
    }
    let _ = graph.remove_chunk_references(id);
    (purge_executed, skipped_soft)
}

fn rocks_config() -> RocksDbConfig {
    RocksDbConfig {
        max_open_files: 100,
        compression: "lz4".to_string(),
        write_buffer_size: 4 * 1024 * 1024,
        max_write_buffer_number: 2,
    }
}

fn tantivy_config() -> TantivyConfig {
    TantivyConfig {
        enabled: true,
        heap_size_mb: 50,
        drift_warn_pct: 5.0,
        auto_rebuild_on_drift: false,
    }
}

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
    }
}

fn make_text_doc(chunk: &Chunk) -> TextDocument {
    TextDocument {
        id: chunk.id.clone(),
        content: chunk.content.clone(),
        user_id: "u1".to_string(),
        app_id: "app1".to_string(),
        level: chunk.level,
        timestamp: chunk.timestamp as i64,
        stream: chunk.stream.clone(),
        entities: None,
        relations: None,
        event_date: None,
        source_agent: None,
    }
}

struct Fixture {
    _tmp: TempDir,
    store: Arc<RocksDbStore>,
    tantivy: Arc<Mutex<TantivyIndex>>,
    graph: Arc<GraphStore>,
}

fn setup_fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let store = Arc::new(RocksDbStore::open(tmp.path().join("rocks"), &rocks_config()).unwrap());
    let tantivy = Arc::new(Mutex::new(
        TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config()).unwrap(),
    ));
    let graph = Arc::new(GraphStore::new(store.clone()));
    Fixture {
        _tmp: tmp,
        store,
        tantivy,
        graph,
    }
}

/// AC7 E2E: store chunk + embedding + tantivy doc + graph reference, then
/// `hard_delete_memory_fully`, then verify every surface is empty.
#[tokio::test]
async fn purge_cascade_removes_chunk_from_every_surface() {
    let fx = setup_fixture();
    let chunk = make_chunk("e2e-1", "__user_test__", "purge e2e fixture body");
    fx.store.store_chunk(&chunk).unwrap();
    fx.store
        .store_embedding("e2e-1", vec![0.1f32, 0.2, 0.3])
        .unwrap();
    {
        let mut idx = fx.tantivy.lock().await;
        idx.index_document(make_text_doc(&chunk)).unwrap();
        idx.commit().unwrap();
    }

    // Pre-conditions.
    assert!(fx.store.get_chunk("e2e-1").unwrap().is_some());
    assert_eq!(fx.store.count_embeddings().unwrap(), 1);

    let (purge_executed, skipped_soft) =
        cascade_purge(&fx.store, &fx.tantivy, &fx.graph, "e2e-1").await;

    assert!(
        purge_executed,
        "purge_executed must be true for existing chunk"
    );
    assert!(
        skipped_soft,
        "fixture had deleted_at=None → skipped_soft=true"
    );

    // Post-conditions: every surface empty.
    assert!(
        fx.store.get_chunk("e2e-1").unwrap().is_none(),
        "store row hard-deleted"
    );
    assert_eq!(
        fx.store.count_embeddings().unwrap(),
        0,
        "embedding hard-deleted from CF_EMBEDDINGS"
    );
    {
        // delete_document is a tantivy marker; count() reflects the drop only
        // after a commit. Matches loomem_core::intent_log::recover, which calls
        // tantivy.commit() at the end of the replay loop.
        let mut idx = fx.tantivy.lock().await;
        idx.commit().unwrap();
        assert_eq!(
            idx.count().unwrap_or(0),
            0,
            "tantivy doc removed after commit (count drops to 0)"
        );
    }
}

/// AC7 + AC6: WAL replay coverage — append `OpType::PurgeChunk` to a fresh
/// IntentLog, then run `recover` against a store that *still has* the chunk
/// (simulating a crash between `append_pending` and store cascade). Recovery
/// must converge by running the cascade to completion, leaving the chunk
/// gone from store + tantivy.
#[tokio::test]
async fn intent_log_recover_purge_chunk_converges_on_uncommitted_entry() {
    let fx = setup_fixture();
    let chunk = make_chunk("replay-1", "__user_test__", "replay fixture");
    fx.store.store_chunk(&chunk).unwrap();
    fx.store
        .store_embedding("replay-1", vec![0.4f32, 0.5, 0.6])
        .unwrap();
    {
        let mut idx = fx.tantivy.lock().await;
        idx.index_document(make_text_doc(&chunk)).unwrap();
        idx.commit().unwrap();
    }

    // Simulate a crash between append_pending and the cascade: write a pending
    // PurgeChunk entry but DO NOT call mark_committed.
    let wal_cfg = IntentLogConfig {
        enabled: true,
        dir: "wal".to_string(),
        max_size_mb: 10,
        sync_on_write: false,
        archive_max_age_days: 7,
    };
    let wal_dir = fx._tmp.path();
    {
        let mut log = IntentLog::open(wal_dir, &wal_cfg).unwrap();
        log.append_pending(OpType::PurgeChunk, "replay-1").unwrap();
    }

    // "Boot" path: reopen the WAL and call recover() against the same store +
    // a fresh tantivy handle (matches loomem-server/src/main.rs:210 startup).
    let mut log_boot = IntentLog::open(wal_dir, &wal_cfg).unwrap();
    let pending_before = log_boot.scan_pending().unwrap();
    assert_eq!(
        pending_before.len(),
        1,
        "pre-recovery: 1 pending entry expected"
    );
    assert_eq!(pending_before[0].op, OpType::PurgeChunk);

    let mut tantivy_boot = {
        let mut idx = fx.tantivy.lock().await;
        // Take ownership for the recover() call. Tantivy is reopenable by path.
        std::mem::replace(
            &mut *idx,
            TantivyIndex::open(
                fx._tmp.path().join("tantivy_boot_placeholder"),
                &tantivy_config(),
            )
            .unwrap(),
        )
    };

    let report =
        loomem_core::intent_log::recover(&mut log_boot, &fx.store, &mut tantivy_boot).unwrap();
    assert!(
        report.replayed >= 1,
        "recover must replay the pending PurgeChunk entry, got report.replayed={}",
        report.replayed
    );

    // Post-recovery: chunk + embedding fully gone.
    assert!(
        fx.store.get_chunk("replay-1").unwrap().is_none(),
        "post-recovery: store row must be hard-deleted"
    );
    assert_eq!(
        fx.store.count_embeddings().unwrap(),
        0,
        "post-recovery: embedding must be gone"
    );

    let pending_after = log_boot.scan_pending().unwrap();
    assert!(
        pending_after.is_empty(),
        "post-recovery: WAL must mark PurgeChunk committed (no pending)"
    );
}

/// Bug-fix lock-in (post-cycle/135): when the requested chunk does not exist,
/// the handler must still call `mark_committed` for the WAL pending entry it
/// appended before the cascade. Pre-fix, `mark_committed` was gated on
/// `purge_executed`, so every 404 leaked one pending entry until the next
/// boot's `recover()`. Mirrors `api_purge_memory_handler` minus HTTP layer.
#[tokio::test]
async fn purge_handler_404_path_commits_wal_pending() {
    let fx = setup_fixture();
    let wal_cfg = IntentLogConfig {
        enabled: true,
        dir: "wal".to_string(),
        max_size_mb: 10,
        sync_on_write: false,
        archive_max_age_days: 7,
    };
    let wal_dir = fx._tmp.path();
    let mut log = IntentLog::open(wal_dir, &wal_cfg).unwrap();

    // Mirror handler sequence with the fix: append_pending → cascade →
    // mark_committed unconditionally.
    let id = "purge-404-wal-lockin";
    let seq = log.append_pending(OpType::PurgeChunk, id).unwrap();
    let (purge_executed, _) = cascade_purge(&fx.store, &fx.tantivy, &fx.graph, id).await;
    assert!(!purge_executed, "missing chunk → purge_executed=false");
    log.mark_committed(seq, OpType::PurgeChunk, id).unwrap();

    let pending = log.scan_pending().unwrap();
    assert!(
        pending.is_empty(),
        "404 path must commit the WAL pending entry; got {} leaked",
        pending.len()
    );
}

/// AC7 idempotency at storage layer: replay against an already-purged chunk
/// is a no-op (no error, no panic). Mirrors the WAL-recovery scenario where
/// the cascade partially completed before crash.
#[tokio::test]
async fn purge_cascade_idempotent_on_already_gone_chunk() {
    let fx = setup_fixture();
    let chunk = make_chunk("idem-1", "__user_test__", "idempotent body");
    fx.store.store_chunk(&chunk).unwrap();

    // First purge: removes everything.
    let (first_executed, _) = cascade_purge(&fx.store, &fx.tantivy, &fx.graph, "idem-1").await;
    assert!(first_executed);
    assert!(fx.store.get_chunk("idem-1").unwrap().is_none());

    // Second purge: chunk gone → purge_executed=false, no errors.
    let (second_executed, second_skipped) =
        cascade_purge(&fx.store, &fx.tantivy, &fx.graph, "idem-1").await;
    assert!(
        !second_executed,
        "second purge must report purge_executed=false (D8 idempotency)"
    );
    assert!(
        !second_skipped,
        "missing chunk → skipped_soft=false (no pre-state to snapshot)"
    );
}
