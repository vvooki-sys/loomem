//! Integration tests: boot recovery for CS2 (dream path) and CS3 (admin reprocess path).
//!
//! These tests verify the functional invariant introduced by cycle /47:
//! for every chunk-persist that registered an intent_log entry, boot recovery
//! (`intent_log::recover`) re-indexes the chunk in Tantivy even if the server
//! crashed between the RocksDB write and the Tantivy write.
//!
//! Test strategy (per AC-4 brief):
//! 1. Write chunk to RocksDB + register intent_log pending entry directly,
//!    but do NOT write to Tantivy (simulates Tantivy-skip / crash mid-write).
//! 2. Assert chunk is in RocksDB, NOT in Tantivy, intent_log entry is pending.
//! 3. Reopen IntentLog (simulates boot).
//! 4. Call `intent_log::recover` with the store + a fresh Tantivy handle.
//! 5. Assert chunk is now searchable in Tantivy and intent_log entry is committed.

use anyhow::Result;
use loomem_core::config::{RocksDbConfig, TantivyConfig};
use loomem_core::intent_log::{IntentLog, IntentLogConfig, OpType};
use loomem_core::storage::rebuild::rebuild_tantivy_if_flag_set;
use loomem_core::storage::Chunk;
use loomem_core::{RocksDbStore, TantivyIndex, TextDocument};
use tempfile::TempDir;

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
        provenance_role: loomem_core::storage::ProvenanceRole::Claim,
    }
}

// ─── Test 1: Dream path (CS2) ─────────────────────────────────────────────────

/// Simulate Tantivy-skip for a dream-path (CS2) chunk:
/// RocksDB write + intent_log pending, Tantivy write skipped.
/// Boot recovery must re-index the chunk.
///
/// This verifies the CS2 fix from cycle /47: when `DreamApplyContext.intent_log`
/// is `Some`, the persist helper registers the pending entry so boot recovery
/// (`intent_log::recover`) can replay the Tantivy write on next startup.
#[tokio::test]
async fn dream_path_recovers_after_simulated_tantivy_skip() -> Result<()> {
    let tmp = TempDir::new()?;

    // Step 1: setup — write chunk to RocksDB + register pending intent_log entry.
    // Tantivy write is intentionally skipped (simulates Tantivy crash mid-write).
    let store = RocksDbStore::open(tmp.path().join("rocks"), &rocksdb_config())?;
    let chunk = make_chunk(
        "dream:recovery-test-001",
        "Anna lives in Krakow since 2020",
        "stream-dream-recovery",
        1,
    );

    {
        let mut ilog = IntentLog::open(tmp.path(), &intent_log_config())?;
        // Register pending intent_log entry (as the helper would before RocksDB write).
        // OpType::Consolidate matches the dream.rs callsite post-/62; recover() is
        // symmetric for Store and Consolidate since /51 PR #106.
        let _seq = ilog.append_pending(OpType::Consolidate, &chunk.id)?;
        // Write to RocksDB only — skip Tantivy (simulated crash between the two writes)
        store.store_chunk(&chunk)?;
        // ilog drops with entry still pending (mark_committed never called)
    }

    // Step 2: assert chunk in RocksDB, NOT in Tantivy, pending entry exists.
    {
        let present = store.get_chunk(&chunk.id)?;
        assert!(present.is_some(), "chunk must be in RocksDB after write");

        // Tantivy is empty — never written to
        let tantivy_raw = TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config())?;
        let results = tantivy_raw.search("Krakow", 10)?;
        assert!(
            results.is_empty(),
            "Tantivy must NOT have chunk before recovery (simulated skip)"
        );

        // intent_log must show pending entry
        let ilog_check = IntentLog::open(tmp.path(), &intent_log_config())?;
        let pending = ilog_check.scan_pending()?;
        assert_eq!(
            pending.len(),
            1,
            "intent_log must have exactly 1 pending entry before recovery"
        );
        assert_eq!(pending[0].id, chunk.id);
        assert_eq!(pending[0].op, OpType::Consolidate);
    }

    // Step 3+4: "boot" — reopen IntentLog and call recover().
    {
        let mut ilog_boot = IntentLog::open(tmp.path(), &intent_log_config())?;
        let mut tantivy_boot = TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config())?;

        let report = loomem_core::intent_log::recover(&mut ilog_boot, &store, &mut tantivy_boot)?;
        assert_eq!(report.replayed, 1, "recovery must replay exactly 1 entry");
        assert_eq!(report.skipped, 0, "recovery must skip 0 entries");

        // Step 5: assert chunk now searchable in Tantivy.
        let results = tantivy_boot.search("Krakow", 10)?;
        assert!(
            !results.is_empty(),
            "chunk must be searchable in Tantivy after recovery"
        );
        assert_eq!(results[0].id, chunk.id, "recovered chunk id must match");

        // intent_log entry must be committed (no more pending).
        let pending_after = ilog_boot.scan_pending()?;
        assert!(
            pending_after.is_empty(),
            "intent_log entry must be committed after recovery"
        );
    }

    Ok(())
}

// ─── Test 2: Admin reprocess path (CS3) ──────────────────────────────────────

/// Simulate Tantivy-skip for an admin-reprocess-path (CS3) chunk:
/// RocksDB write + intent_log pending, Tantivy write skipped.
/// Boot recovery must re-index the chunk.
///
/// This verifies the CS3 fix from cycle /47: the admin reprocess handler now
/// passes `intent_log: state_clone.intent_log.as_deref()` instead of `None`,
/// so chunks written in the reprocess batch are covered by boot recovery.
/// `op: OpType::Store` mirrors the actual CS3 usage.
#[tokio::test]
async fn admin_reprocess_path_recovers_after_simulated_tantivy_skip() -> Result<()> {
    let tmp = TempDir::new()?;

    // Step 1: setup — RocksDB write + intent_log pending, no Tantivy write.
    let store = RocksDbStore::open(tmp.path().join("rocks"), &rocksdb_config())?;
    let chunk = make_chunk(
        "admin-reprocess-recovery-001",
        "Legacy raw chunk about project deadline",
        "stream-admin-recovery",
        0,
    );

    {
        let mut ilog = IntentLog::open(tmp.path(), &intent_log_config())?;
        let _seq = ilog.append_pending(OpType::Store, &chunk.id)?;
        store.store_chunk(&chunk)?;
        // ilog drops — pending entry persists to disk
    }

    // Step 2: assert pre-recovery state.
    {
        assert!(
            store.get_chunk(&chunk.id)?.is_some(),
            "chunk must be in RocksDB"
        );

        let tantivy_raw = TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config())?;
        let results = tantivy_raw.search("deadline", 10)?;
        assert!(
            results.is_empty(),
            "Tantivy must NOT have chunk before recovery"
        );

        let ilog_check = IntentLog::open(tmp.path(), &intent_log_config())?;
        let pending = ilog_check.scan_pending()?;
        assert_eq!(pending.len(), 1, "must have 1 pending entry");
        assert_eq!(pending[0].op, OpType::Store);
        assert_eq!(pending[0].id, chunk.id);
    }

    // Steps 3+4: boot recovery.
    {
        let mut ilog_boot = IntentLog::open(tmp.path(), &intent_log_config())?;
        let mut tantivy_boot = TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config())?;

        let report = loomem_core::intent_log::recover(&mut ilog_boot, &store, &mut tantivy_boot)?;
        assert_eq!(report.replayed, 1, "recovery must replay 1 entry");
        assert_eq!(report.skipped, 0, "recovery must skip 0 entries");

        // Step 5: chunk searchable post-recovery.
        let results = tantivy_boot.search("deadline", 10)?;
        assert!(
            !results.is_empty(),
            "chunk must be searchable after recovery"
        );
        assert_eq!(results[0].id, chunk.id);

        let pending_after = ilog_boot.scan_pending()?;
        assert!(
            pending_after.is_empty(),
            "intent_log must have no pending entries after recovery"
        );
    }

    Ok(())
}

// ─── Test 3: Consolidation L1 path (CS4) ─────────────────────────────────────

/// Simulate Tantivy-skip for a consolidation L1 chunk:
/// RocksDB write + intent_log pending, Tantivy write skipped.
/// Boot recovery must re-index the L1 chunk.
///
/// This verifies the /48 fix: consolidation.rs L1 path now calls
/// `persist_chunk_with_index` with `intent_log: Some`. Before /48 the Tantivy
/// block was warn-skip and mark_committed fired regardless of Tantivy outcome,
/// causing boot recovery to see the entry as committed and skip Tantivy replay
/// → permanent BM25 miss.
///
/// Post-/62 the callsite uses OpType::Consolidate (semantically accurate).
/// recover() is symmetric for Store and Consolidate since /51 PR #106.
#[tokio::test]
async fn consolidation_l1_path_recovers_after_simulated_tantivy_skip() -> Result<()> {
    let tmp = TempDir::new()?;

    // Step 1: setup — write L1 chunk to RocksDB + register pending intent_log
    // entry with OpType::Consolidate, but skip Tantivy write (simulates crash
    // between RocksDB write and Tantivy commit inside persist_chunk_with_index).
    let store = RocksDbStore::open(tmp.path().join("rocks"), &rocksdb_config())?;
    let chunk = make_chunk(
        "L1:consolidation-recovery-test-001",
        "Consolidated memory: Piotr moved to Warsaw in 2024 and started new job",
        "stream-consolidation-recovery",
        1, // L1 chunk
    );

    {
        let mut ilog = IntentLog::open(tmp.path(), &intent_log_config())?;
        // Register pending intent_log entry with OpType::Consolidate, matching
        // the /62 callsite. recover() is symmetric since /51 PR #106.
        let _seq = ilog.append_pending(OpType::Consolidate, &chunk.id)?;
        // Write to RocksDB only — Tantivy write skipped (simulated crash)
        store.store_chunk(&chunk)?;
        // ilog drops — pending entry stays on disk (mark_committed never called)
    }

    // Step 2: assert pre-recovery state: chunk in RocksDB, NOT in Tantivy, pending.
    {
        let present = store.get_chunk(&chunk.id)?;
        assert!(
            present.is_some(),
            "L1 chunk must be in RocksDB after consolidation write"
        );

        let tantivy_raw = TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config())?;
        let results = tantivy_raw.search("Warsaw", 10)?;
        assert!(
            results.is_empty(),
            "Tantivy must NOT have L1 chunk before recovery (simulated Tantivy skip)"
        );

        let ilog_check = IntentLog::open(tmp.path(), &intent_log_config())?;
        let pending = ilog_check.scan_pending()?;
        assert_eq!(
            pending.len(),
            1,
            "intent_log must have exactly 1 pending entry before recovery"
        );
        assert_eq!(pending[0].id, chunk.id);
        assert_eq!(
            pending[0].op,
            OpType::Consolidate,
            "pending entry op must be Consolidate (post-/62 callsite)"
        );
    }

    // Steps 3+4: "boot" — reopen IntentLog and call recover().
    {
        let mut ilog_boot = IntentLog::open(tmp.path(), &intent_log_config())?;
        let mut tantivy_boot = TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config())?;

        let report = loomem_core::intent_log::recover(&mut ilog_boot, &store, &mut tantivy_boot)?;
        assert_eq!(
            report.replayed, 1,
            "recovery must replay exactly 1 L1 entry"
        );
        assert_eq!(report.skipped, 0, "recovery must skip 0 entries");

        // Step 5: assert L1 chunk is now searchable in Tantivy after recovery.
        let results = tantivy_boot.search("Warsaw", 10)?;
        assert!(
            !results.is_empty(),
            "L1 chunk must be searchable in Tantivy after boot recovery"
        );
        assert_eq!(results[0].id, chunk.id, "recovered L1 chunk id must match");

        // intent_log entry must be committed — no more pending.
        let pending_after = ilog_boot.scan_pending()?;
        assert!(
            pending_after.is_empty(),
            "intent_log must have no pending entries after recovery"
        );
    }

    Ok(())
}

// ─── Test 4: OpType::Consolidate symmetry (cycle /51) ────────────────────────

/// Verify that OpType::Consolidate pending entries trigger Tantivy re-index on recovery.
///
/// Pre-/51 this test would FAIL: the Consolidate branch only marked committed
/// without calling tantivy.upsert_document, so the chunk would remain missing
/// from BM25 search after recovery.
///
/// Post-/51: the Consolidate branch mirrors the Store branch — when the chunk
/// exists in RocksDB, build_recovery_text_doc + upsert_document is called.
#[tokio::test]
async fn consolidate_op_recovers_tantivy_too() -> Result<()> {
    let tmp = TempDir::new()?;

    // Step 1: setup — write L1 chunk to RocksDB + register pending intent_log
    // entry with OpType::Consolidate, but skip Tantivy write entirely.
    // This simulates a crash between the RocksDB write and the Tantivy write
    // in a hypothetical path using OpType::Consolidate.
    let store = RocksDbStore::open(tmp.path().join("rocks"), &rocksdb_config())?;
    let chunk = make_chunk(
        "L1:consolidate-symmetry-test-001",
        "Consolidated: Marek started cycling to work in spring 2024",
        "stream-consolidate-symmetry",
        1,
    );

    {
        let mut ilog = IntentLog::open(tmp.path(), &intent_log_config())?;
        // Register pending with OpType::Consolidate (the asymmetric op type).
        let _seq = ilog.append_pending(OpType::Consolidate, &chunk.id)?;
        // Write to RocksDB only — Tantivy write skipped (simulated crash).
        store.store_chunk(&chunk)?;
        // ilog drops — pending entry persists, mark_committed never called.
    }

    // Step 2: assert pre-recovery state.
    {
        let present = store.get_chunk(&chunk.id)?;
        assert!(present.is_some(), "chunk must be in RocksDB after write");

        // Tantivy is empty — never written to.
        let tantivy_raw = TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config())?;
        let results = tantivy_raw.search("cycling", 10)?;
        assert!(
            results.is_empty(),
            "Tantivy must NOT have chunk before recovery (simulated skip)"
        );

        // intent_log must show pending Consolidate entry.
        let ilog_check = IntentLog::open(tmp.path(), &intent_log_config())?;
        let pending = ilog_check.scan_pending()?;
        assert_eq!(
            pending.len(),
            1,
            "intent_log must have exactly 1 pending entry before recovery"
        );
        assert_eq!(pending[0].id, chunk.id);
        assert_eq!(
            pending[0].op,
            OpType::Consolidate,
            "pending entry op must be Consolidate"
        );
    }

    // Steps 3+4: "boot" — reopen IntentLog and call recover().
    {
        let mut ilog_boot = IntentLog::open(tmp.path(), &intent_log_config())?;
        let mut tantivy_boot = TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config())?;

        let report = loomem_core::intent_log::recover(&mut ilog_boot, &store, &mut tantivy_boot)?;

        // Step 7: assert recovery replayed 1 entry, skipped 0.
        assert_eq!(
            report.replayed, 1,
            "recovery must replay exactly 1 Consolidate entry"
        );
        assert_eq!(report.skipped, 0, "recovery must skip 0 entries");

        // Step 8: chunk must now be searchable in Tantivy (was missing pre-/51).
        let results = tantivy_boot.search("cycling", 10)?;
        assert!(
            !results.is_empty(),
            "L1 chunk must be searchable in Tantivy after recovery (Consolidate symmetry)"
        );
        assert_eq!(results[0].id, chunk.id, "recovered chunk id must match");

        // Step 9: intent_log entry must be committed — no more pending.
        let pending_after = ilog_boot.scan_pending()?;
        assert!(
            pending_after.is_empty(),
            "intent_log must have no pending entries after recovery"
        );
    }

    Ok(())
}

/// Verify that OpType::Consolidate recovery is idempotent: if the chunk is
/// already in Tantivy (crash after upsert_document but before mark_committed),
/// recovery calls upsert_document again and produces exactly 1 doc, not 2.
#[tokio::test]
async fn consolidate_op_idempotent_with_existing_tantivy_doc() -> Result<()> {
    let tmp = TempDir::new()?;

    let store = RocksDbStore::open(tmp.path().join("rocks"), &rocksdb_config())?;
    let mut tantivy_setup = TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config())?;

    let chunk = make_chunk(
        "L1:consolidate-idempotent-test-001",
        "Consolidated: Zofia completed her PhD thesis on urban planning",
        "stream-consolidate-idempotent",
        1,
    );

    // Step 1: chunk in both RocksDB and Tantivy (crash after tantivy.upsert_document
    // but before mark_committed). intent_log entry still pending.
    {
        let mut ilog = IntentLog::open(tmp.path(), &intent_log_config())?;
        let _seq = ilog.append_pending(OpType::Consolidate, &chunk.id)?;
        store.store_chunk(&chunk)?;
        // Write to Tantivy as well (unlike the previous test).
        tantivy_setup.upsert_document(TextDocument {
            id: chunk.id.clone(),
            content: chunk.content.clone(),
            user_id: String::new(),
            app_id: String::new(),
            level: chunk.level,
            stream: chunk.stream.clone(),
            timestamp: chunk.timestamp as i64,
            entities: None,
            relations: None,
            event_date: None,
            source_agent: None,
        })?;
        tantivy_setup.commit()?;
        // ilog drops — pending entry persists (mark_committed never called).
    }
    drop(tantivy_setup);

    // Pre-recovery: chunk is in Tantivy, pending entry exists.
    {
        let tantivy_check = TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config())?;
        let results = tantivy_check.search("PhD", 10)?;
        assert_eq!(
            results.len(),
            1,
            "Tantivy must have exactly 1 doc before recovery"
        );
    }

    // Boot recovery.
    {
        let mut ilog_boot = IntentLog::open(tmp.path(), &intent_log_config())?;
        let mut tantivy_boot = TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config())?;

        let report = loomem_core::intent_log::recover(&mut ilog_boot, &store, &mut tantivy_boot)?;
        assert_eq!(report.replayed, 1, "recovery must replay 1 entry");
        assert_eq!(report.skipped, 0, "recovery must skip 0");

        // Post-recovery: still exactly 1 doc (upsert is idempotent).
        let results = tantivy_boot.search("PhD", 10)?;
        assert_eq!(
            results.len(),
            1,
            "Tantivy must have exactly 1 doc after recovery (upsert idempotency)"
        );
        assert_eq!(results[0].id, chunk.id);

        let pending_after = ilog_boot.scan_pending()?;
        assert!(
            pending_after.is_empty(),
            "intent_log must have no pending entries after recovery"
        );
    }

    Ok(())
}

// ─── Test 4: meta:tantivy_rebuild_needed flag (cycle /49) ────────────────────

/// Simulate the migrate-shared-stream scenario: chunks are stored in both
/// RocksDB and Tantivy with stream="user_a", then RocksDB chunk.stream is
/// restamped to "shared" (as loomem-migrate does). Tantivy is stale.
///
/// Verifies the cycle /49 invariant: setting meta:tantivy_rebuild_needed=1
/// and calling rebuild_tantivy_if_flag_set triggers a full rebuild, after
/// which search with stream="shared" returns the migrated chunks.
#[tokio::test]
async fn tantivy_rebuild_needed_flag_triggers_full_rebuild() -> Result<()> {
    let tmp = TempDir::new()?;

    // Step 1+2: setup — write 3 chunks to RocksDB and Tantivy with stream="user_a".
    let store = RocksDbStore::open(tmp.path().join("rocks"), &rocksdb_config())?;
    let mut tantivy = TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config())?;

    let ts = now_secs();
    let chunks: Vec<Chunk> = (0..3)
        .map(|i| {
            make_chunk(
                &format!("migrate-test-{i:03}"),
                &format!("Migration test content number {i}"),
                "user_a",
                0,
            )
        })
        .collect();

    for chunk in &chunks {
        store.store_chunk(chunk)?;
        tantivy.upsert_document(TextDocument {
            id: chunk.id.clone(),
            content: chunk.content.clone(),
            user_id: "default".to_string(),
            app_id: "default".to_string(),
            level: chunk.level,
            timestamp: ts as i64,
            stream: chunk.stream.clone(), // "user_a"
            entities: None,
            relations: None,
            event_date: None,
            source_agent: None,
        })?;
    }
    tantivy.commit()?;

    // Step 3: restamp chunks in RocksDB to stream="shared" (simulates migrate).
    // Tantivy still has them tagged stream="user_a" (stale).
    let mut restamped = chunks.clone();
    for c in &mut restamped {
        c.stream = "shared".to_string();
        store.store_chunk(c)?;
    }

    // Step 4: set the rebuild flag.
    store.set_tantivy_rebuild_needed(true)?;

    // Step 5: Tantivy still stale — search for stream="shared" returns 0 hits.
    let pre_results = tantivy.search_with_stream("Migration test", "shared", 10)?;
    assert!(
        pre_results.is_empty(),
        "Tantivy must return 0 hits for stream=shared before rebuild (stale)"
    );

    // Step 6: call rebuild helper (mirrors server boot path).
    let rebuilt = rebuild_tantivy_if_flag_set(&store, &mut tantivy)?;
    assert!(
        rebuilt,
        "rebuild_tantivy_if_flag_set must return true when flag was set"
    );

    // Step 7: after rebuild, stream="shared" returns 3 hits.
    let post_results = tantivy.search_with_stream("Migration test", "shared", 10)?;
    assert_eq!(
        post_results.len(),
        3,
        "Tantivy must return 3 hits for stream=shared after rebuild"
    );

    // Step 8: flag must be cleared.
    assert!(
        !store.get_tantivy_rebuild_needed()?,
        "meta:tantivy_rebuild_needed must be false after rebuild"
    );

    // Bonus: stream="user_a" returns 0 hits (no longer tagged there).
    let old_stream_results = tantivy.search_with_stream("Migration test", "user_a", 10)?;
    assert!(
        old_stream_results.is_empty(),
        "Tantivy must return 0 hits for old stream=user_a after rebuild"
    );

    Ok(())
}
