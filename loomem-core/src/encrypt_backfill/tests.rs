use super::*;
use crate::crypto::provider::MasterKeyEnvProvider;
use crate::graph::GraphStore;
use crate::storage::{Chunk, RocksDbStore};
use crate::{backfill_trace::TraceLog, storage::RocksDbConfig};
use std::sync::Arc;
use tempfile::TempDir;

fn test_config() -> RocksDbConfig {
    RocksDbConfig {
        max_open_files: 100,
        compression: "none".to_string(),
        write_buffer_size: 4 * 1024 * 1024,
        max_write_buffer_number: 2,
    }
}

fn open_noop(tmp: &TempDir) -> RocksDbStore {
    RocksDbStore::open(tmp.path(), &test_config()).expect("open noop")
}

fn open_encrypted(tmp: &TempDir) -> Arc<RocksDbStore> {
    let store = RocksDbStore::open(tmp.path(), &test_config()).expect("open encrypted");
    let provider = Arc::new(MasterKeyEnvProvider::new([7u8; 32], store.db_arc()));
    Arc::new(store.with_encryption_provider(provider))
}

fn make_chunk(id: &str, stream: &str) -> Chunk {
    Chunk {
        id: id.to_string(),
        content: format!("content-{id}"),
        stream: stream.to_string(),
        level: 0,
        score: 1.0,
        timestamp: 1_000_000,
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

fn params(batch_size: usize) -> EncryptBackfillParams {
    EncryptBackfillParams {
        snapshot_token: "snap-20260101-test-aabbccdd".to_string(),
        batch_size,
        inter_batch_sleep_ms: 0,
    }
}

fn trace(tmp: &TempDir) -> TraceLog {
    TraceLog::new(tmp.path().to_str().expect("utf8 path"))
}

// T4 (AC-6 core): NoopProvider → run_encrypt_backfill returns Err.
#[tokio::test]
async fn t4_noop_provider_is_refused() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Arc::new(open_noop(&tmp));
    let graph = Arc::new(GraphStore::new(store.clone()));
    let trace = trace(&tmp);

    let result = run_encrypt_backfill(&store, &graph, &params(200), &trace).await;
    assert!(result.is_err(), "NoopProvider must return Err");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("NoopProvider") || msg.contains("disabled"),
        "error must mention disabled provider: {msg}"
    );
}

// T1 (AC-1/AC-4): write legacy rows via noop, switch to encrypted provider,
// run backfill, verify all rows encrypted. Run again → encrypted==0.
#[tokio::test]
async fn t1_idempotent_full_coverage() {
    let tmp = TempDir::new().expect("tempdir");
    let stream = "__test_backfill_stream__";

    // Write legacy plaintext rows through NoopProvider (simulates pre-key data).
    // Scoped block ensures the noop store is dropped (DB lock released) before
    // we reopen the same directory with the encrypted provider.
    {
        let store_noop = Arc::new(open_noop(&tmp));
        let graph_noop = Arc::new(GraphStore::new(store_noop.clone()));

        store_noop
            .store_chunk(&make_chunk("bf_c0", stream))
            .expect("store chunk L0");
        let mut c1 = make_chunk("bf_c1", stream);
        c1.level = 1;
        store_noop.store_chunk(&c1).expect("store chunk L1");

        graph_noop
            .get_or_create_entity("TestEntity", "Person", &[], stream)
            .expect("create entity");

        store_noop
            .store_entities(
                "bf_c0",
                stream,
                &[("Alice".to_string(), "Person".to_string())],
            )
            .expect("store entities");
        store_noop
            .store_relations(
                "bf_c0",
                stream,
                &[("Alice".to_string(), "knows".to_string(), "Bob".to_string())],
            )
            .expect("store relations");
        store_noop
            .append_audit("testuser", 1_000_000, 0, b"audit-event-1")
            .expect("append audit");
        store_noop
            .append_access(stream, 1_000_001, 0, b"access-event-1")
            .expect("append access");
    } // store_noop + graph_noop dropped here — DB lock released.

    // Open the same directory with the encrypted provider.
    let store_enc = open_encrypted(&tmp);
    let graph_enc = Arc::new(GraphStore::new(store_enc.clone()));
    let trace = trace(&tmp);

    // Run #1.
    let prog = run_encrypt_backfill(&store_enc, &graph_enc, &params(200), &trace)
        .await
        .expect("run #1 ok");
    assert_eq!(prog.status, "completed", "run #1 must complete");

    // Verify chunk:L0 is now encrypted.
    let raw = store_enc
        .db()
        .get(b"chunk:L0:bf_c0")
        .expect("db get")
        .expect("key present");
    // StoredChunkRead envelope: encrypted_payload non-empty.
    let staged: StoredChunkRead = serde_json::from_slice(&raw).expect("deserialize chunk envelope");
    assert!(
        !staged.encrypted_payload.is_empty(),
        "chunk L0 must be encrypted after backfill"
    );

    // Verify entity: row is encrypted (magic prefix).
    let entity_raw = store_enc
        .db()
        .get(b"entity:bf_c0")
        .expect("db get")
        .expect("key present");
    assert!(
        is_encrypted(&entity_raw),
        "entity: row must be encrypted after backfill"
    );

    // Verify audit: row is encrypted.
    let audit_prefix = b"audit:testuser:";
    let audit_row = store_enc
        .prefix_scan(audit_prefix)
        .next()
        .expect("audit row present");
    assert!(is_encrypted(&audit_row.1), "audit row must be encrypted");

    // Verify access: row is encrypted.
    let access_prefix = format!("access:{stream}:");
    let access_row = store_enc
        .prefix_scan(access_prefix.as_bytes())
        .next()
        .expect("access row present");
    assert!(is_encrypted(&access_row.1), "access row must be encrypted");

    // Check per-class encrypted counter > 0 for at least chunks.
    let chunk_l0 = prog.per_class.get("chunk_L0").expect("class present");
    assert!(
        chunk_l0.encrypted >= 1,
        "at least one chunk L0 must have been encrypted"
    );

    // Run #2 — idempotency: all rows already encrypted.
    let prog2 = run_encrypt_backfill(&store_enc, &graph_enc, &params(200), &trace)
        .await
        .expect("run #2 ok");
    assert_eq!(prog2.status, "completed", "run #2 must complete");
    let c0_2 = prog2.per_class.get("chunk_L0").expect("class present");
    assert_eq!(c0_2.encrypted, 0, "run #2 must write nothing for chunk_L0");
    assert!(
        c0_2.already_encrypted >= 1,
        "run #2 must count chunk_L0 as already_encrypted"
    );
}

// T2 (AC-5): round-trip — decode_chunk/get_entities/get_relations/
// scan_audit/scan_access after backfill returns byte-identical content.
#[tokio::test]
async fn t2_round_trip_content() {
    let tmp = TempDir::new().expect("tempdir");
    let stream = "__rt_stream__";

    // Noop writes in scoped block to release DB lock before reopening.
    {
        let store_noop = open_noop(&tmp);
        let mut chunk = make_chunk("rt_c1", stream);
        chunk.content = "round trip content".to_string();
        store_noop.store_chunk(&chunk).expect("store chunk");
        store_noop
            .store_entities(
                "rt_c1",
                stream,
                &[("Alice".to_string(), "Person".to_string())],
            )
            .expect("store entities");
        store_noop
            .store_relations(
                "rt_c1",
                stream,
                &[("A".to_string(), "knows".to_string(), "B".to_string())],
            )
            .expect("store relations");
        store_noop
            .append_audit("rt_user", 2_000_000, 0, b"rt-audit")
            .expect("append audit");
        store_noop
            .append_access(stream, 2_000_001, 0, b"rt-access")
            .expect("append access");
    } // noop lock released.

    let store_enc = open_encrypted(&tmp);
    let graph_enc = Arc::new(GraphStore::new(store_enc.clone()));
    let trace = trace(&tmp);
    run_encrypt_backfill(&store_enc, &graph_enc, &params(200), &trace)
        .await
        .expect("backfill ok");

    // Chunk content round-trip.
    let got = store_enc
        .get_chunk("rt_c1")
        .expect("get_chunk ok")
        .expect("present");
    assert_eq!(
        got.content, "round trip content",
        "chunk content round-trip"
    );

    // Entities.
    let ents = store_enc
        .get_entities("rt_c1", stream)
        .expect("get_entities");
    assert!(
        ents.iter().any(|e| e.contains("Alice")),
        "Alice must survive round-trip"
    );

    // Relations.
    let rels = store_enc
        .get_relations("rt_c1", stream)
        .expect("get_relations");
    assert_eq!(rels.len(), 1);
    assert_eq!(rels[0].1, "knows");

    // Audit.
    let (audit_events, dropped) = store_enc.scan_audit("rt_user", 100);
    assert_eq!(dropped, 0, "no undecryptable audit rows");
    assert_eq!(audit_events, vec![b"rt-audit".to_vec()]);

    // Access.
    let (access_records, dropped) = store_enc.scan_access(stream, 100);
    assert_eq!(dropped, 0, "no undecryptable access rows");
    assert_eq!(access_records, vec![b"rt-access".to_vec()]);
}

// T3c (D10): per-scope hard threshold — pure counter logic, no storage needed.
// Orphans with a known scope stop the run at ≥50 in one class·scope even when
// the per-class total is below 100; unknown-scope orphans stop only at 100.
#[test]
fn t3c_per_scope_threshold_logic() {
    let mut counters = ClassCounters::default();
    for i in 0..49 {
        assert!(
            !record_orphan("test_class", Some("scope-a"), &mut counters),
            "orphan #{} in scope-a must not stop yet",
            i + 1
        );
    }
    assert!(
        record_orphan("test_class", Some("scope-a"), &mut counters),
        "50th orphan in scope-a must stop the run"
    );
    assert_eq!(counters.orphans, 50);
    assert_eq!(counters.orphans_by_scope.get("scope-a"), Some(&50));
    assert!(
        orphan_limits_hit(&counters),
        "end-of-class predicate agrees"
    );

    // Unknown-scope orphans: per-class threshold (100) governs.
    let mut counters = ClassCounters::default();
    for i in 0..99 {
        assert!(
            !record_orphan("test_class", None, &mut counters),
            "unknown-scope orphan #{} must not stop yet",
            i + 1
        );
    }
    assert!(
        record_orphan("test_class", None, &mut counters),
        "100th unknown-scope orphan must stop the run"
    );
    assert!(counters.orphans_by_scope.is_empty());
}

// T3 (AC-7): orphan counting — entity/rel without paired chunk.
#[tokio::test]
async fn t3_orphan_counting_no_paired_chunk() {
    let tmp = TempDir::new().expect("tempdir");
    let stream = "__orphan_stream__";

    {
        let store_noop = open_noop(&tmp);
        store_noop
            .store_entities(
                "ghost_chunk",
                stream,
                &[("Ghost".to_string(), "Person".to_string())],
            )
            .expect("store entities");
    } // noop lock released.

    let store_enc = open_encrypted(&tmp);
    let graph_enc = Arc::new(GraphStore::new(store_enc.clone()));
    let trace = trace(&tmp);

    let prog = run_encrypt_backfill(&store_enc, &graph_enc, &params(200), &trace)
        .await
        .expect("run ok");
    assert_eq!(prog.status, "completed", "below threshold so run completes");
    let entity_c = prog.per_class.get("entity").expect("entity class present");
    assert_eq!(entity_c.orphans, 1, "one orphan for missing paired chunk");

    // The row must be unchanged (still plaintext).
    let raw = store_enc
        .db()
        .get(b"entity:ghost_chunk")
        .expect("db get")
        .expect("key present");
    assert!(
        !is_encrypted(&raw),
        "orphaned entity: row must remain plaintext"
    );
}

// T3b: hard orphan threshold → stopped_orphan_threshold + classes after stop untouched.
#[tokio::test]
async fn t3b_hard_orphan_threshold_stops_run() {
    let tmp = TempDir::new().expect("tempdir");
    let stream = "__stop_stream__";

    {
        let store_noop = open_noop(&tmp);
        // Write 100 entity: rows without paired chunks — exactly the hard threshold.
        for i in 0..100 {
            let chunk_id = format!("ghost_{i:04}");
            store_noop
                .store_entities(
                    &chunk_id,
                    stream,
                    &[("G".to_string(), "Person".to_string())],
                )
                .expect("store entities");
        }
        // Write a chunk (processed before entity: — verifiable post-stop).
        store_noop
            .store_chunk(&make_chunk("after_stop", stream))
            .expect("store chunk");
    } // noop lock released.

    let store_enc = open_encrypted(&tmp);
    let graph_enc = Arc::new(GraphStore::new(store_enc.clone()));
    let trace = trace(&tmp);

    // Small batch to force multiple flush iterations.
    let prog = run_encrypt_backfill(&store_enc, &graph_enc, &params(10), &trace)
        .await
        .expect("run ok (returns BackfillProgress)");

    assert_eq!(
        prog.status, "stopped_orphan_threshold",
        "run must stop at threshold: {prog:?}"
    );
    let entity_c = prog.per_class.get("entity").expect("entity class");
    assert!(
        entity_c.orphans >= ORPHAN_STOP_PER_CLASS,
        "orphan count must reach threshold"
    );
    // rel: and audit: classes must be absent from per_class (not started).
    assert!(
        !prog.per_class.contains_key("rel"),
        "rel class must not be started after stop"
    );
}

// T5 (resume): simulate interrupted run via orphan-stop, then full run with
// no orphans → all rows encrypted, zero double-encryption.
#[tokio::test]
async fn t5_resume_after_stop() {
    let tmp = TempDir::new().expect("tempdir");
    let stream = "__resume_stream__";

    // Noop writes in scoped block to release DB lock before reopening.
    {
        let store_noop = open_noop(&tmp);
        // Write a chunk (will be encrypted in the first run before entity: stop).
        store_noop
            .store_chunk(&make_chunk("resume_c0", stream))
            .expect("store chunk");
        // Write audit: row — will NOT be encrypted in run #1 (stop before audit:).
        store_noop
            .append_audit("resume_user", 3_000_000, 0, b"resume-audit")
            .expect("append audit");
        // Write 100 entity: orphans to trigger stop.
        for i in 0..100 {
            store_noop
                .store_entities(
                    &format!("orphan_r_{i:04}"),
                    stream,
                    &[("G".to_string(), "Person".to_string())],
                )
                .expect("store entities");
        }
    } // noop lock released.

    let store_enc = open_encrypted(&tmp);
    let graph_enc = Arc::new(GraphStore::new(store_enc.clone()));
    let trace = trace(&tmp);

    // Run #1: stops at entity: orphan threshold.
    let prog1 = run_encrypt_backfill(&store_enc, &graph_enc, &params(10), &trace)
        .await
        .expect("run #1 ok");
    assert_eq!(prog1.status, "stopped_orphan_threshold");

    // chunk:L0 was processed before entity: — must be encrypted.
    let chunk_raw = store_enc
        .db()
        .get(b"chunk:L0:resume_c0")
        .expect("db get")
        .expect("present");
    let staged: StoredChunkRead = serde_json::from_slice(&chunk_raw).expect("parse envelope");
    assert!(
        !staged.encrypted_payload.is_empty(),
        "chunk must be encrypted after run #1"
    );

    // Remove orphan rows so run #2 can complete.
    for i in 0..100 {
        store_enc
            .delete(format!("entity:orphan_r_{i:04}").as_bytes())
            .expect("delete orphan");
    }

    // Run #2: completes fully.
    let prog2 = run_encrypt_backfill(&store_enc, &graph_enc, &params(200), &trace)
        .await
        .expect("run #2 ok");
    assert_eq!(prog2.status, "completed");

    // chunk:L0 must be already_encrypted in run #2 (not re-encrypted).
    let c0 = prog2.per_class.get("chunk_L0").expect("chunk_L0 class");
    assert_eq!(c0.encrypted, 0, "chunk must not be re-encrypted in run #2");
    assert!(
        c0.already_encrypted >= 1,
        "chunk must be already_encrypted in run #2"
    );

    // audit: row must now be encrypted.
    let (events, dropped) = store_enc.scan_audit("resume_user", 100);
    assert_eq!(dropped, 0);
    assert_eq!(events, vec![b"resume-audit".to_vec()]);
}
