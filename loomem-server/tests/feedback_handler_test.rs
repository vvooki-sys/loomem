//! Cycle/112: integration test for `POST /v1/feedback` write-side.
//!
//! Exercises `FeedbackService` end-to-end with a real tempdir-backed
//! RocksDB. Mirrors the handler's loop semantics (per-rating validate +
//! apply, aggregate `accepted` and `rejected[]`) so the same outcomes a
//! live HTTP request would produce are observed here.
//!
//! HTTP-layer wiring (axum router, auth middleware, JSON serde) is
//! covered separately by the in-process `mod tests` block in
//! `loomem-server/src/main.rs`. This file is the integration-tests
//! crate entry — loomem-server has no `[lib]` target, so cross-crate
//! HTTP-level setup is not reachable from here without exposing the
//! whole server library; the service-layer integration is what AC-14
//! gates against (`cargo test --test feedback_handler_test`).

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
    }
}

/// Replicates the handler's per-rating loop. Returns `(accepted, rejected)`.
fn run_request(
    svc: &FeedbackService<'_>,
    caller_stream: &str,
    ratings: &[(String, u8, bool, String)],
) -> (u32, Vec<(String, String)>) {
    let now_ms = 1_700_000_000_000;
    let mut accepted: u32 = 0;
    let mut rejected: Vec<(String, String)> = Vec::new();

    for (i, (chunk_id, usefulness, harmful, justification)) in ratings.iter().enumerate() {
        let event_id = format!("evt-{i}");
        let args = ApplyRatingArgs {
            chunk_id,
            usefulness: *usefulness,
            harmful: *harmful,
            justification,
            caller_stream,
            caller_is_admin: false,
            agent_id: caller_stream,
            model_version: "integration-test",
            prompt_version: "integration-v1",
            trajectory_id: None,
            now_unix_ms: now_ms,
            event_id: &event_id,
        };
        match svc.apply_rating(args).expect("apply_rating ok") {
            RatingOutcome::Accepted => accepted += 1,
            RatingOutcome::Rejected { chunk_id, reason } => rejected.push((chunk_id, reason)),
        }
    }
    (accepted, rejected)
}

#[test]
fn two_valid_plus_one_cross_stream_yields_accepted_2_and_one_rejection() {
    let (store, _tmp) = fresh_store();
    let cfg = FeedbackConfig::default();
    let svc = FeedbackService::new(&store, &cfg);

    // Caller "agent_s1" owns stream "s1"; chunks c1, c2 live there.
    store.store_chunk(&make_chunk("c1", "s1")).unwrap();
    store.store_chunk(&make_chunk("c2", "s1")).unwrap();
    // c_other lives in a different stream — caller cannot see it.
    store
        .store_chunk(&make_chunk("c_other", "stream_other"))
        .unwrap();

    let ratings = vec![
        ("c1".to_string(), 4, false, "valuable".to_string()),
        ("c2".to_string(), 2, false, "ok-ish".to_string()),
        (
            "c_other".to_string(),
            4,
            false,
            "tried cross-stream".to_string(),
        ),
    ];
    let (accepted, rejected) = run_request(&svc, "s1", &ratings);

    assert_eq!(accepted, 2, "two in-stream ratings should be accepted");
    assert_eq!(rejected.len(), 1, "one cross-stream rating rejected");
    assert_eq!(rejected[0].0, "c_other");
    assert_eq!(rejected[0].1, "not_found_in_stream");

    // Verify in-stream chunks were updated and cross-stream chunk was not.
    let c1 = store.get_chunk("c1").unwrap().expect("c1");
    let c2 = store.get_chunk("c2").unwrap().expect("c2");
    let c_other = store.get_chunk("c_other").unwrap().expect("c_other");

    assert_eq!(c1.n_ratings, 1);
    assert!((c1.alpha - 2.0).abs() < 1e-9, "c1 alpha 1+4/4=2");
    assert_eq!(c2.n_ratings, 1);
    assert!((c2.alpha - 1.5).abs() < 1e-9, "c2 alpha 1+2/4=1.5");
    assert_eq!(c_other.n_ratings, 0, "cross-stream chunk untouched");
    assert_eq!(c_other.last_rated_at, None);
}

#[test]
fn happy_path_one_rating_persists_event_and_aggregate() {
    let (store, _tmp) = fresh_store();
    let cfg = FeedbackConfig::default();
    let svc = FeedbackService::new(&store, &cfg);

    store.store_chunk(&make_chunk("c1", "s1")).unwrap();

    let ratings = vec![("c1".to_string(), 4, false, "happy".to_string())];
    let (accepted, rejected) = run_request(&svc, "s1", &ratings);
    assert_eq!(accepted, 1);
    assert!(rejected.is_empty());

    let events = svc.query_events_for_chunk("c1").unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].usefulness, 4);
    assert!(!events[0].harmful);
    assert_eq!(events[0].stream, "s1");
}

#[test]
fn malformed_inputs_rejected_by_validate_rating() {
    let (store, _tmp) = fresh_store();
    let cfg = FeedbackConfig::default();
    let svc = FeedbackService::new(&store, &cfg);

    // 1. usefulness out of range
    assert!(svc.validate_rating(99, false, "x").is_err());
    // 2. empty justification
    assert!(svc.validate_rating(0, false, "").is_err());
    // 3. harmful=true with empty justification
    assert!(svc.validate_rating(0, true, "").is_err());
    // 4. justification too long
    let too_long = "x".repeat(cfg.max_justification_chars + 1);
    assert!(svc.validate_rating(2, false, &too_long).is_err());
    // 5. boundary success — usefulness=0 with non-empty justification is fine
    assert!(svc.validate_rating(0, false, "boundary").is_ok());
}
