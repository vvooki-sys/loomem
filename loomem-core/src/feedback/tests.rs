//! Cycle/112: unit tests for the feedback service.
//!
//! All tests use tempdir-backed RocksDB — no mocks, no sleeps (CLAUDE.md §6).

use tempfile::TempDir;

use crate::storage::{Chunk, RocksDbConfig, RocksDbStore};

use super::config::FeedbackConfig;
use super::event::{event_key, event_prefix_for_chunk, FeedbackEvent};
use super::service::{update_chunk_tally, ApplyRatingArgs, FeedbackService, RatingOutcome};

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
        provenance_role: crate::storage::ProvenanceRole::Claim,
    }
}

fn args<'a>(
    chunk_id: &'a str,
    usefulness: u8,
    harmful: bool,
    justification: &'a str,
    caller_stream: &'a str,
) -> ApplyRatingArgs<'a> {
    ApplyRatingArgs {
        chunk_id,
        usefulness,
        harmful,
        justification,
        caller_stream,
        caller_is_admin: false,
        agent_id: caller_stream,
        model_version: "test-model",
        prompt_version: "test-prompt",
        trajectory_id: None,
        now_unix_ms: 1_700_000_000_000,
        event_id: "event-1",
    }
}

#[test]
fn apply_rating_happy_path_updates_tally_and_logs_event() {
    let (store, _tmp) = fresh_store();
    let cfg = FeedbackConfig::default();
    let svc = FeedbackService::new(&store, &cfg);

    let chunk = make_chunk("c1", "s1");
    store.store_chunk(&chunk).unwrap();

    let outcome = svc
        .apply_rating(args("c1", 4, false, "good", "s1"))
        .expect("apply");
    assert!(matches!(outcome, RatingOutcome::Accepted));

    let after = store.get_chunk("c1").unwrap().expect("present");
    assert!((after.alpha - 2.0).abs() < 1e-9, "alpha=2 (1+4/4)");
    assert!((after.beta - 1.0).abs() < 1e-9, "beta=1 (1+(4-4)/4)");
    assert_eq!(after.harmful_count, 0);
    assert_eq!(after.n_ratings, 1);
    assert_eq!(after.last_rated_at, Some(1_700_000_000_000));

    let events = svc.query_events_for_chunk("c1").unwrap();
    assert_eq!(events.len(), 1, "exactly one logged event");
    assert_eq!(events[0].usefulness, 4);
    assert!(!events[0].harmful);
}

#[test]
fn apply_rating_harmful_adds_penalty_and_counter() {
    let (store, _tmp) = fresh_store();
    let cfg = FeedbackConfig::default();
    let svc = FeedbackService::new(&store, &cfg);

    let chunk = make_chunk("c2", "s1");
    store.store_chunk(&chunk).unwrap();

    let outcome = svc
        .apply_rating(args("c2", 0, true, "misleading", "s1"))
        .expect("apply");
    assert!(matches!(outcome, RatingOutcome::Accepted));

    let after = store.get_chunk("c2").unwrap().expect("present");
    // alpha += 0/4 = 0 → stays 1.0
    // beta  += (4-0)/4 = 1.0; harmful → += 4.0 → 1 + 1 + 4 = 6.0
    assert!(
        (after.alpha - 1.0).abs() < 1e-9,
        "alpha unchanged on usefulness=0"
    );
    assert!((after.beta - 6.0).abs() < 1e-9, "beta = 1 + 1 + 4");
    assert_eq!(after.harmful_count, 1);
    assert_eq!(after.n_ratings, 1);
}

#[test]
fn apply_rating_scope_mismatch_rejects_with_not_found_reason() {
    let (store, _tmp) = fresh_store();
    let cfg = FeedbackConfig::default();
    let svc = FeedbackService::new(&store, &cfg);

    let chunk = make_chunk("c3", "stream_owner");
    store.store_chunk(&chunk).unwrap();

    let outcome = svc
        .apply_rating(args("c3", 4, false, "ok", "stream_other"))
        .expect("apply");
    match outcome {
        RatingOutcome::Rejected { chunk_id, reason } => {
            assert_eq!(chunk_id, "c3");
            assert_eq!(reason, "not_found_in_stream");
        }
        RatingOutcome::Accepted => panic!("expected Rejected for cross-stream caller"),
    }

    // Chunk state untouched.
    let after = store.get_chunk("c3").unwrap().expect("present");
    assert_eq!(after.n_ratings, 0);
    assert!((after.alpha - 1.0).abs() < 1e-9);
}

#[test]
fn validate_rating_rejects_malformed_inputs() {
    let (store, _tmp) = fresh_store();
    let cfg = FeedbackConfig::default();
    let svc = FeedbackService::new(&store, &cfg);

    assert!(svc.validate_rating(5, false, "x").is_err(), "usefulness=5");
    assert!(
        svc.validate_rating(99, false, "x").is_err(),
        "usefulness=99"
    );
    assert!(
        svc.validate_rating(2, false, "").is_err(),
        "empty justification"
    );
    assert!(
        svc.validate_rating(0, true, "").is_err(),
        "harmful=true empty"
    );
    let too_long = "x".repeat(cfg.max_justification_chars + 1);
    assert!(
        svc.validate_rating(3, false, &too_long).is_err(),
        "too long"
    );

    // Happy edges
    assert!(svc.validate_rating(0, false, "ok").is_ok());
    assert!(svc.validate_rating(4, false, "ok").is_ok());
    assert!(svc.validate_rating(0, true, "harm").is_ok());
}

#[test]
fn write_batch_is_atomic_chunk_and_event_both_readable() {
    let (store, _tmp) = fresh_store();
    let cfg = FeedbackConfig::default();
    let svc = FeedbackService::new(&store, &cfg);

    let chunk = make_chunk("c4", "s1");
    store.store_chunk(&chunk).unwrap();

    svc.apply_rating(args("c4", 3, false, "ok", "s1"))
        .expect("apply");

    // Chunk read: aggregate updated.
    let updated = store.get_chunk("c4").unwrap().expect("present");
    assert_eq!(updated.n_ratings, 1);

    // Event read: exactly one event under the expected key prefix.
    let prefix = event_prefix_for_chunk("c4");
    let mut hits = 0;
    for (k, v) in store.prefix_scan(prefix.as_bytes()) {
        hits += 1;
        let key_str = String::from_utf8_lossy(&k).into_owned();
        assert!(key_str.starts_with("feedback:c4:"), "key shape");
        let _ev: FeedbackEvent = serde_json::from_slice(&v).expect("decode event");
    }
    assert_eq!(hits, 1);
}

// /157 finding 1: a feedback rating must not rewrite an encrypted chunk row
// back to legacy plaintext. `write_batch_atomic` routes through `encode_chunk`
// — the same envelope as `store_chunk` (/138 §D). Raw-bytes pattern follows
// storage.rs::chunk_field_level_encryption_roundtrip.
#[test]
fn apply_rating_preserves_field_level_encryption() {
    use crate::crypto::at_rest::MAGIC;
    use crate::crypto::provider::MasterKeyEnvProvider;
    use std::sync::Arc;

    let tmp = TempDir::new().expect("tempdir");
    let store = RocksDbStore::open(tmp.path(), &rocksdb_cfg()).expect("rocksdb open");
    let provider = Arc::new(MasterKeyEnvProvider::new([7u8; 32], store.db_arc()));
    let store = store.with_encryption_provider(provider);
    let cfg = FeedbackConfig::default();
    let svc = FeedbackService::new(&store, &cfg);

    let mut chunk = make_chunk("c-enc", "s1");
    chunk.content = "secret content".to_string();
    store.store_chunk(&chunk).expect("store_chunk");

    let outcome = svc
        .apply_rating(args("c-enc", 4, false, "good", "s1"))
        .expect("apply");
    assert!(matches!(outcome, RatingOutcome::Accepted));

    // Raw on-disk row after the rating: still the encrypted envelope —
    // plaintext fields cleared, encrypted_payload present and AES-GCM-shaped.
    let key = format!("chunk:L{}:{}", chunk.level, chunk.id);
    let raw = store
        .db()
        .get(key.as_bytes())
        .expect("get")
        .expect("present");

    #[derive(serde::Deserialize)]
    struct Envelope {
        content: String,
        metadata: Option<serde_json::Value>,
        encrypted_payload: Option<Vec<u8>>,
    }
    let env: Envelope = serde_json::from_slice(&raw).expect("deserialize envelope");
    assert_eq!(env.content, "", "plaintext content must stay cleared");
    assert!(env.metadata.is_none());
    let ep = env
        .encrypted_payload
        .expect("encrypted_payload must survive the rating");
    assert_eq!(&ep[..4], &MAGIC[..], "payload is an AES-GCM blob");

    // Decode path: tally updated AND content decrypts back to the original.
    let after = store.get_chunk("c-enc").unwrap().expect("present");
    assert_eq!(after.n_ratings, 1);
    assert_eq!(after.content, "secret content");

    // Event written in the same batch.
    let events = svc.query_events_for_chunk("c-enc").unwrap();
    assert_eq!(events.len(), 1, "exactly one logged event");
}

#[test]
fn event_key_is_deterministic_and_prefixable() {
    let k = event_key("abc", 12345, "evt");
    assert_eq!(k, "feedback:abc:12345:evt");
    let p = event_prefix_for_chunk("abc");
    assert_eq!(p, "feedback:abc:");
    assert!(k.starts_with(&p));
}

#[test]
fn update_chunk_tally_pure_math() {
    let mut c = make_chunk("p1", "s1");
    update_chunk_tally(&mut c, 4, false, 7);
    assert!((c.alpha - 2.0).abs() < 1e-9);
    assert!((c.beta - 1.0).abs() < 1e-9);
    assert_eq!(c.n_ratings, 1);
    assert_eq!(c.last_rated_at, Some(7));

    update_chunk_tally(&mut c, 0, true, 8);
    // alpha unchanged (usefulness=0 → 0/4 added)
    // beta: 1 (start) + 1 (4/4 from this call) + 4 (harmful penalty) = 6
    assert!((c.alpha - 2.0).abs() < 1e-9);
    assert!((c.beta - 6.0).abs() < 1e-9);
    assert_eq!(c.harmful_count, 1);
    assert_eq!(c.n_ratings, 2);
    assert_eq!(c.last_rated_at, Some(8));
}

#[test]
fn legacy_chunk_json_deserializes_with_default_priors() {
    // A chunk written before /112 lacks all five new fields.
    // `#[serde(default)]` must yield alpha=1.0, beta=1.0, zeros + None.
    let json = serde_json::json!({
        "id": "legacy",
        "content": "hello",
        "stream": "s1",
        "level": 0,
        "score": 0.5,
        "timestamp": 1000,
        "consolidated": false,
        "dormant": false,
        "in_progress": false,
        "prompt_version": null,
        "source_ids": null,
        "last_decay": null,
        "metadata": null
    });
    let bytes = serde_json::to_vec(&json).unwrap();
    let chunk: Chunk = serde_json::from_slice(&bytes).expect("legacy deserialize");
    assert!((chunk.alpha - 1.0).abs() < 1e-9);
    assert!((chunk.beta - 1.0).abs() < 1e-9);
    assert_eq!(chunk.harmful_count, 0);
    assert_eq!(chunk.n_ratings, 0);
    assert_eq!(chunk.last_rated_at, None);
}
