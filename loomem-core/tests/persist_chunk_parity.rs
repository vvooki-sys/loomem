//! Parity tests for `storage::persist::persist_chunk_with_index` (cycle /46).
//!
//! Verifies that the helper produces the same observable state as the pre-/46
//! per-callsite logic at each of the three migrated callsites (CS1, CS2, CS3),
//! and that a Tantivy failure leaves the intent_log entry pending (rollback
//! semantics documented in persist.rs).

use anyhow::Result;
use loomem_core::config::{RocksDbConfig, TantivyConfig};
use loomem_core::intent_log::{IntentLog, IntentLogConfig, OpType};
use loomem_core::storage::{persist_chunk_with_index, Chunk, PersistChunkArgs};
use loomem_core::{RocksDbStore, TantivyIndex, TextDocument};
use tempfile::TempDir;
use tokio::sync::Mutex;

// ─── helpers ─────────────────────────────────────────────────────────────────

fn rocksdb_config() -> RocksDbConfig {
    RocksDbConfig {
        max_open_files: 50,
        compression: "lz4".to_string(),
        write_buffer_size: 4 * 1024 * 1024,
        max_write_buffer_number: 2,
    }
}

fn tantivy_config() -> TantivyConfig {
    TantivyConfig {
        enabled: true,
        heap_size_mb: 16,
        drift_warn_pct: 5.0,
        auto_rebuild_on_drift: false,
    }
}

fn intent_log_config() -> IntentLogConfig {
    IntentLogConfig {
        enabled: true,
        dir: "wal".to_string(),
        max_size_mb: 10,
        sync_on_write: false,
        archive_max_age_days: 7,
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn make_chunk(id: &str, content: &str, stream: &str, level: i32) -> Chunk {
    Chunk {
        id: id.to_string(),
        content: content.to_string(),
        stream: stream.to_string(),
        level,
        score: 1.0,
        timestamp: now_secs(),
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
        user_id: "test-user".to_string(),
        app_id: "test-app".to_string(),
        level: chunk.level,
        timestamp: chunk.timestamp as i64,
        stream: chunk.stream.clone(),
        entities: None,
        relations: None,
        event_date: None,
        source_agent: None,
    }
}

// ─── CS1: ingest path — full intent_log pattern ──────────────────────────────

/// Mirrors the ingest callsite: chunk persisted with intent_log present.
/// Asserts: RocksDB has chunk, Tantivy has chunk, intent_log entry committed.
#[tokio::test]
async fn parity_ingest_path() -> Result<()> {
    let tmp = TempDir::new()?;
    let store = RocksDbStore::open(tmp.path().join("rocks"), &rocksdb_config())?;
    let tantivy_raw = TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config())?;
    let tantivy = Mutex::new(tantivy_raw);
    let ilog = IntentLog::open(tmp.path(), &intent_log_config())?;
    let ilog_mutex = Mutex::new(ilog);

    let chunk = make_chunk(
        "ingest-001",
        "The quick brown fox jumps over the lazy dog",
        "stream-cs1",
        0,
    );
    let text_doc = make_text_doc(&chunk);

    persist_chunk_with_index(
        &store,
        &tantivy,
        PersistChunkArgs {
            chunk: &chunk,
            text_doc,
            intent_log: Some(&ilog_mutex),
            op: OpType::Store,
        },
    )
    .await?;

    // Assert 1: chunk present in RocksDB under expected key
    let stored = store.get_chunk("ingest-001")?;
    assert!(stored.is_some(), "chunk must be in RocksDB");
    assert_eq!(
        stored.unwrap().content,
        "The quick brown fox jumps over the lazy dog"
    );

    // Assert 2: chunk searchable in Tantivy
    let results = tantivy.lock().await.search("fox", 10)?;
    assert!(!results.is_empty(), "chunk must be searchable in Tantivy");
    assert_eq!(results[0].id, "ingest-001");

    // Assert 3: intent_log entry committed (no pending entries)
    let pending = ilog_mutex.lock().await.scan_pending()?;
    assert!(
        pending.is_empty(),
        "intent_log entry must be committed, not pending"
    );

    Ok(())
}

// ─── CS2: dream path — no intent_log ────────────────────────────────────────

/// Mirrors the dream callsite: L1 chunk persisted without intent_log.
/// Asserts: RocksDB has chunk at L1 key, Tantivy has chunk.
#[tokio::test]
async fn parity_dream_path() -> Result<()> {
    let tmp = TempDir::new()?;
    let store = RocksDbStore::open(tmp.path().join("rocks"), &rocksdb_config())?;
    let tantivy_raw = TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config())?;
    let tantivy = Mutex::new(tantivy_raw);

    let chunk = make_chunk(
        "dream:abc-123",
        "Anna lives in Krakow since 2020",
        "stream-cs2",
        1,
    );
    let text_doc = TextDocument {
        id: chunk.id.clone(),
        content: chunk.content.clone(),
        user_id: "dream".to_string(),
        app_id: "dream".to_string(),
        level: chunk.level,
        timestamp: chunk.timestamp as i64,
        stream: chunk.stream.clone(),
        entities: Some("Anna,Krakow".to_string()),
        relations: None,
        event_date: None,
        source_agent: Some("dream-consolidation".to_string()),
    };

    persist_chunk_with_index(
        &store,
        &tantivy,
        PersistChunkArgs {
            chunk: &chunk,
            text_doc,
            intent_log: None,
            op: OpType::Consolidate,
        },
    )
    .await?;

    // Assert 1: chunk present in RocksDB at L1 key
    let stored = store.get_chunk("dream:abc-123")?;
    assert!(stored.is_some(), "dream chunk must be in RocksDB");
    assert_eq!(stored.unwrap().level, 1);

    // Assert 2: chunk searchable in Tantivy
    let results = tantivy.lock().await.search("Krakow", 10)?;
    assert!(
        !results.is_empty(),
        "dream chunk must be searchable in Tantivy"
    );
    assert_eq!(results[0].id, "dream:abc-123");

    Ok(())
}

// ─── CS3: admin reprocess path — no intent_log, result ignored ───────────────

/// Mirrors the admin reprocess callsite: a modified chunk (updated source/
/// importance) is re-persisted. No intent_log. Caller ignores Result with
/// `let _ = ...` (FIXME(cycle/A)).
/// Asserts: RocksDB has updated chunk, Tantivy has chunk.
#[tokio::test]
async fn parity_admin_reprocess_path() -> Result<()> {
    let tmp = TempDir::new()?;
    let store = RocksDbStore::open(tmp.path().join("rocks"), &rocksdb_config())?;
    let tantivy_raw = TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config())?;
    let tantivy = Mutex::new(tantivy_raw);

    let mut chunk = make_chunk(
        "admin-reprocess-001",
        "legacy raw content about project deadline",
        "stream-cs3",
        0,
    );
    chunk.source = Some(loomem_core::SourceTag::from_agent("legacy-raw"));
    chunk.importance = Some(0.3);

    let text_doc = TextDocument {
        id: chunk.id.clone(),
        content: chunk.content.clone(),
        user_id: "default".to_string(),
        app_id: "admin-reprocess".to_string(),
        level: chunk.level,
        timestamp: chunk.timestamp as i64,
        stream: chunk.stream.clone(),
        entities: None,
        relations: None,
        event_date: None,
        source_agent: chunk.source.as_ref().map(|s| s.agent.clone()),
    };

    // CS3 semantics: result is ignored via let _ = (bug preserved per /46 refactor-pure)
    let _ = persist_chunk_with_index(
        &store,
        &tantivy,
        PersistChunkArgs {
            chunk: &chunk,
            text_doc,
            intent_log: None,
            op: OpType::Store,
        },
    )
    .await;

    // Assert 1: chunk present in RocksDB
    let stored = store.get_chunk("admin-reprocess-001")?;
    assert!(stored.is_some(), "admin reprocess chunk must be in RocksDB");
    let stored_chunk = stored.unwrap();
    assert_eq!(
        stored_chunk.source.as_ref().map(|s| s.agent.as_str()),
        Some("legacy-raw")
    );

    // Assert 2: chunk searchable in Tantivy
    let results = tantivy.lock().await.search("deadline", 10)?;
    assert!(
        !results.is_empty(),
        "admin reprocess chunk must be searchable in Tantivy"
    );
    assert_eq!(results[0].id, "admin-reprocess-001");

    Ok(())
}

// ─── CS3 idempotency: reprocess must not duplicate Tantivy doc ───────────────

/// Regression test for H1 (cycle /46 critic): calling the helper twice with
/// the same chunk_id but a mutated `source_agent` must produce exactly 1
/// Tantivy document, not 2 (pre-fix `index_document` was append-only and would
/// silently create a duplicate BM25 doc on every reprocess pass).
///
/// Assertion sequence:
/// 1. First helper call — assert Tantivy doc count == 1.
/// 2. Second helper call (same id, different source_agent) — assert Tantivy doc
///    count STILL == 1.
/// 3. Search by content — assert exactly 1 hit (not 2).
#[tokio::test]
async fn parity_admin_reprocess_idempotent() -> Result<()> {
    let tmp = TempDir::new()?;
    let store = RocksDbStore::open(tmp.path().join("rocks"), &rocksdb_config())?;
    let tantivy_raw = TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config())?;
    let tantivy = Mutex::new(tantivy_raw);

    let chunk = make_chunk(
        "reprocess-idem-001",
        "content about idempotent reprocess",
        "stream-idem",
        0,
    );

    // First call: initial ingest path
    let text_doc_first = TextDocument {
        id: chunk.id.clone(),
        content: chunk.content.clone(),
        user_id: "default".to_string(),
        app_id: "test".to_string(),
        level: chunk.level,
        timestamp: chunk.timestamp as i64,
        stream: chunk.stream.clone(),
        entities: None,
        relations: None,
        event_date: None,
        source_agent: Some("original-agent".to_string()),
    };
    persist_chunk_with_index(
        &store,
        &tantivy,
        PersistChunkArgs {
            chunk: &chunk,
            text_doc: text_doc_first,
            intent_log: None,
            op: OpType::Store,
        },
    )
    .await?;

    // After first call: exactly 1 Tantivy document
    let results_after_first = tantivy.lock().await.search("idempotent reprocess", 10)?;
    assert_eq!(
        results_after_first.len(),
        1,
        "exactly 1 Tantivy doc after first call"
    );

    // Second call: reprocess with same chunk_id but different source_agent
    let text_doc_second = TextDocument {
        id: chunk.id.clone(),
        content: chunk.content.clone(),
        user_id: "default".to_string(),
        app_id: "test".to_string(),
        level: chunk.level,
        timestamp: chunk.timestamp as i64,
        stream: chunk.stream.clone(),
        entities: None,
        relations: None,
        event_date: None,
        source_agent: Some("reprocessed-agent".to_string()),
    };
    persist_chunk_with_index(
        &store,
        &tantivy,
        PersistChunkArgs {
            chunk: &chunk,
            text_doc: text_doc_second,
            intent_log: None,
            op: OpType::Store,
        },
    )
    .await?;

    // After second call: STILL exactly 1 Tantivy document (upsert, not append)
    let results_after_second = tantivy.lock().await.search("idempotent reprocess", 10)?;
    assert_eq!(
        results_after_second.len(),
        1,
        "exactly 1 Tantivy doc after second call — upsert must replace, not append"
    );
    assert_eq!(
        results_after_second[0].id, "reprocess-idem-001",
        "the single hit must have the correct chunk id"
    );

    Ok(())
}

// ─── rollback on Tantivy fail ────────────────────────────────────────────────

/// Verifies that when Tantivy write fails, the intent_log entry stays pending
/// (not committed) — ready for replay on next restart.
///
/// To simulate Tantivy failure without a mock layer, we close the Tantivy
/// index directory after creating the Mutex, making any write fail with
/// an IO error. RocksDB write will have succeeded (chunk may be in RocksDB).
/// We assert: helper returns Err, intent_log entry stays pending.
#[tokio::test]
async fn rollback_on_tantivy_fail() -> Result<()> {
    let tmp = TempDir::new()?;
    let store = RocksDbStore::open(tmp.path().join("rocks"), &rocksdb_config())?;

    // Open Tantivy in a separate subdirectory we can remove to force failure
    let tantivy_dir = tmp.path().join("tantivy-fail");
    std::fs::create_dir_all(&tantivy_dir)?;
    let tantivy_raw = TantivyIndex::open(&tantivy_dir, &tantivy_config())?;
    let tantivy = Mutex::new(tantivy_raw);

    let ilog = IntentLog::open(tmp.path(), &intent_log_config())?;
    let ilog_mutex = Mutex::new(ilog);

    let chunk = make_chunk(
        "rollback-001",
        "content that will fail tantivy",
        "stream-rollback",
        0,
    );

    // Force Tantivy failure by removing the index directory files
    // so the commit step fails with an IO error.
    for entry in std::fs::read_dir(&tantivy_dir)? {
        let entry = entry?;
        let _ = std::fs::remove_file(entry.path());
    }
    std::fs::remove_dir_all(&tantivy_dir)?;

    let text_doc = make_text_doc(&chunk);
    let result = persist_chunk_with_index(
        &store,
        &tantivy,
        PersistChunkArgs {
            chunk: &chunk,
            text_doc,
            intent_log: Some(&ilog_mutex),
            op: OpType::Store,
        },
    )
    .await;

    // Assert 1: helper returns Err
    assert!(result.is_err(), "helper must return Err when Tantivy fails");

    // Assert 2: intent_log entry stays pending (NOT committed)
    // The chunk is in RocksDB (acceptable per refactor-pure rollback semantics)
    // but the log entry must remain pending for replay.
    let pending = ilog_mutex.lock().await.scan_pending()?;
    assert_eq!(
        pending.len(),
        1,
        "intent_log entry must remain pending when Tantivy write fails"
    );
    assert_eq!(pending[0].id, "rollback-001");

    Ok(())
}

// ─── entity-less parity (source-provenance-fixes Issue 2) ────────────────────

/// Issue 2: after removing the entity gate, `retag_all_handler` re-indexes
/// EVERY chunk — entity-less ones included — exactly like `persist_chunk`. This
/// locks the persist side of that parity: an entity-less chunk (`entities` +
/// `relations` both `None`) is indexed into Tantivy, and reprocessing it with a
/// mutated `source_agent` (what a retag sweep does) leaves exactly one doc
/// (upsert, not append). That is the Tantivy state retag must now reproduce for
/// entity-less chunks instead of silently skipping them.
#[tokio::test]
async fn parity_entity_less_chunk_reindexed_idempotent() -> Result<()> {
    let tmp = TempDir::new()?;
    let store = RocksDbStore::open(tmp.path().join("rocks"), &rocksdb_config())?;
    let tantivy_raw = TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config())?;
    let tantivy = Mutex::new(tantivy_raw);

    let chunk = make_chunk(
        "entity-less-001",
        "plain note without any extractable entities",
        "stream-entityless",
        0,
    );

    // Entity-less document: entities + relations are None, source_agent set.
    let make_doc = |agent: &str| TextDocument {
        id: chunk.id.clone(),
        content: chunk.content.clone(),
        user_id: "default".to_string(),
        app_id: "test".to_string(),
        level: chunk.level,
        timestamp: chunk.timestamp as i64,
        stream: chunk.stream.clone(),
        entities: None,
        relations: None,
        event_date: None,
        source_agent: Some(agent.to_string()),
    };

    // First index — initial ingest equivalent.
    persist_chunk_with_index(
        &store,
        &tantivy,
        PersistChunkArgs {
            chunk: &chunk,
            text_doc: make_doc("agent-one"),
            intent_log: None,
            op: OpType::Store,
        },
    )
    .await?;
    let after_first = tantivy.lock().await.search("plain note", 10)?;
    assert_eq!(
        after_first.len(),
        1,
        "entity-less chunk must be indexed by the shared persist path"
    );
    assert_eq!(after_first[0].id, "entity-less-001");

    // Reprocess with a mutated source_agent — mirrors a retag sweep pass.
    persist_chunk_with_index(
        &store,
        &tantivy,
        PersistChunkArgs {
            chunk: &chunk,
            text_doc: make_doc("agent-two"),
            intent_log: None,
            op: OpType::Store,
        },
    )
    .await?;
    let after_second = tantivy.lock().await.search("plain note", 10)?;
    assert_eq!(
        after_second.len(),
        1,
        "reprocess must upsert, not append, for entity-less chunks"
    );
    assert_eq!(after_second[0].id, "entity-less-001");

    Ok(())
}
