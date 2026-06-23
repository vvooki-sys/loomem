//! Cycle/113: integration test for MCP `memory_feedback` tool.
//!
//! Exercises `FeedbackService` end-to-end with a real tempdir-backed
//! RocksDB, replicating the control flow that `tool_feedback` in
//! `dispatcher.rs` would execute:
//!   validate_rating → apply_rating → RatingOutcome
//!
//! Three scenarios: happy path, validation reject, cross-stream reject.
//! See brief §5 / AC-13.
//!
//! loomem-server has no `[lib]` target, so HTTP-layer dispatch is not
//! reachable from here. The service-layer integration is what AC-13 gates
//! against (`cargo test --test feedback_mcp_test`).

use tempfile::TempDir;

use loomem_core::feedback::{ApplyRatingArgs, FeedbackConfig, FeedbackService, RatingOutcome};
use loomem_core::storage::{Chunk, RocksDbConfig, RocksDbStore};

fn rocksdb_cfg() -> RocksDbConfig {
    RocksDbConfig {
        max_open_files: 50,
        compression: "none".into(),
        write_buffer_size: 4 * 1024 * 1024,
        max_write_buffer_number: 2,
    }
}

fn fresh_store() -> (RocksDbStore, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let store = RocksDbStore::open(tmp.path(), &rocksdb_cfg()).expect("rocksdb open");
    (store, tmp)
}

fn make_chunk(id: &str, stream: &str) -> Chunk {
    Chunk {
        id: id.to_string(),
        content: "content".to_string(),
        stream: stream.to_string(),
        level: 0,
        score: 1.0,
        timestamp: 1000,
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

/// Happy path: usefulness=4, harmful=false, valid chunk in caller's stream.
/// Mirrors the tool_feedback success branch — validate_rating ok → apply_rating
/// → Accepted → {"ok": true, "accepted": 1, "rejected": []}.
#[test]
fn happy_path_usefulness_4_accepted() {
    let (store, _tmp) = fresh_store();
    let cfg = FeedbackConfig::default();
    let svc = FeedbackService::new(&store, &cfg);

    store
        .store_chunk(&make_chunk("chunk-a", "stream-s1"))
        .unwrap();

    // Validate first (as tool_feedback does)
    assert!(
        svc.validate_rating(
            4,
            false,
            "Without this chunk I could not have completed the task"
        )
        .is_ok(),
        "validation must pass for usefulness=4"
    );

    let outcome = svc
        .apply_rating(ApplyRatingArgs {
            chunk_id: "chunk-a",
            usefulness: 4,
            harmful: false,
            justification: "Without this chunk I could not have completed the task",
            caller_stream: "stream-s1",
            caller_is_admin: false,
            agent_id: "agent-test",
            model_version: "claude-sonnet-4-6",
            prompt_version: "loomem-feedback-v1",
            trajectory_id: None,
            now_unix_ms: 1_700_000_000_000,
            event_id: "evt-happy-001",
        })
        .expect("apply_rating ok");

    assert!(
        matches!(outcome, RatingOutcome::Accepted),
        "expected Accepted, got Rejected"
    );

    // Verify chunk tally was updated
    let chunk = store
        .get_chunk("chunk-a")
        .unwrap()
        .expect("chunk must exist");
    assert_eq!(chunk.n_ratings, 1);
    // alpha starts at 1.0; usefulness=4 → alpha += 4.0/4.0 = 1.0 → alpha = 2.0
    assert!(
        (chunk.alpha - 2.0).abs() < 1e-9,
        "alpha should be 2.0 after usefulness=4"
    );
}

/// Validation reject: usefulness=99 is out of range 0..=4.
/// Mirrors the tool_feedback validation branch — validate_rating Err →
/// ToolResult::error("validation failed: ...").
#[test]
fn validate_reject_usefulness_out_of_range() {
    let (store, _tmp) = fresh_store();
    let cfg = FeedbackConfig::default();
    let svc = FeedbackService::new(&store, &cfg);

    let result = svc.validate_rating(99, false, "x");
    assert!(result.is_err(), "usefulness=99 must fail validation");
    let err = result.unwrap_err();
    assert!(
        err.contains("99") && err.contains("out of range"),
        "error message must mention 99 and 'out of range', got: {err}"
    );
}

/// Cross-stream reject: chunk belongs to another stream.
/// Mirrors the tool_feedback apply_rating branch — Rejected { reason: "not_found_in_stream" }
/// → {"ok": true, "accepted": 0, "rejected": [{"chunk_id": ..., "reason": ...}]}.
#[test]
fn cross_stream_reject_not_found_in_stream() {
    let (store, _tmp) = fresh_store();
    let cfg = FeedbackConfig::default();
    let svc = FeedbackService::new(&store, &cfg);

    // chunk lives in stream-other, caller is stream-s1
    store
        .store_chunk(&make_chunk("chunk-other", "stream-other"))
        .unwrap();

    // Validate passes (valid input)
    assert!(svc
        .validate_rating(3, false, "Helpful for general context")
        .is_ok());

    let outcome = svc
        .apply_rating(ApplyRatingArgs {
            chunk_id: "chunk-other",
            usefulness: 3,
            harmful: false,
            justification: "Helpful for general context",
            caller_stream: "stream-s1",
            caller_is_admin: false,
            agent_id: "agent-test",
            model_version: "claude-sonnet-4-6",
            prompt_version: "loomem-feedback-v1",
            trajectory_id: None,
            now_unix_ms: 1_700_000_000_000,
            event_id: "evt-cross-001",
        })
        .expect("apply_rating ok");

    match outcome {
        RatingOutcome::Rejected { chunk_id, reason } => {
            assert_eq!(chunk_id, "chunk-other");
            assert_eq!(reason, "not_found_in_stream");
        }
        RatingOutcome::Accepted => panic!("expected Rejected for cross-stream chunk"),
    }

    // Cross-stream chunk must remain untouched
    let chunk = store
        .get_chunk("chunk-other")
        .unwrap()
        .expect("chunk must still exist");
    assert_eq!(chunk.n_ratings, 0, "cross-stream chunk tally must be zero");
    assert!(chunk.last_rated_at.is_none());
}
