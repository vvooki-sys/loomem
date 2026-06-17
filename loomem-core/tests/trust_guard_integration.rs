//! Integration tests for the trust hierarchy guard (cycle /40).
//!
//! Covers `contradiction::apply_supersede` and the dream-path
//! `contradiction::try_supersede_with_guard`. Trust ranks: a1 > a2 > b.
//! Guard rule: lower-trust content cannot supersede higher-trust content.
//! On block, an audit entry with `action: "trust_guard_blocked"` is appended
//! for the old chunk's stream.

use anyhow::Result;
use loomem_core::audit;
use loomem_core::config::{RocksDbConfig, TantivyConfig};
use loomem_core::contradiction::{apply_supersede, try_supersede_with_guard};
use loomem_core::storage::Chunk;
use loomem_core::{RocksDbStore, TantivyIndex};
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

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build a Chunk with the given trust level for stream `s`. `is_latest=true`,
/// `version=1`, no version-chain links — the bare minimum the guard cares
/// about plus stream/id for audit lookup.
fn chunk_with_trust(id: &str, stream: &str, trust: Option<&str>) -> Chunk {
    Chunk {
        id: id.to_string(),
        content: format!("content for {id}"),
        stream: stream.to_string(),
        level: 0,
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
        trust_level: trust.map(|s| s.to_string()),
        ingester_user_id: None,

        alpha: 1.0,
        beta: 1.0,
        harmful_count: 0,
        n_ratings: 0,
        last_rated_at: None,
    }
}

/// Open a RocksDbStore and seed the `old` chunk under `stream`.
fn setup(stream: &str, old_trust: Option<&str>) -> Result<(TempDir, RocksDbStore, Chunk)> {
    let temp = TempDir::new()?;
    let store = RocksDbStore::open(temp.path().join("rocks"), &rocksdb_config())?;
    let old = chunk_with_trust("chunk-old", stream, old_trust);
    store.store_chunk(&old)?;
    Ok((temp, store, old))
}

fn count_trust_guard_blocked(store: &RocksDbStore, stream: &str) -> usize {
    audit::list(store, stream, usize::MAX)
        .map(|l| l.events)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.action == "trust_guard_blocked")
        .count()
}

// ─── apply_supersede: positive cases (truth table allows) ────────────────────

#[test]
fn test_a1_can_supersede_a1_via_apply_supersede() -> Result<()> {
    let (_tmp, store, old) = setup("s_a1_a1", Some("a1"))?;
    let new = chunk_with_trust("chunk-new", "s_a1_a1", Some("a1"));
    let updated = apply_supersede(&store, &old, new)?;

    let refreshed = store.get_chunk(&old.id)?.expect("old chunk persists");
    assert!(!refreshed.is_latest, "old.is_latest must flip");
    assert_eq!(refreshed.superseded_by.as_deref(), Some("chunk-new"));
    assert_eq!(updated.supersedes_id.as_deref(), Some("chunk-old"));
    assert_eq!(updated.version, 2);
    assert_eq!(count_trust_guard_blocked(&store, "s_a1_a1"), 0);
    Ok(())
}

#[test]
fn test_a1_can_supersede_a2_via_apply_supersede() -> Result<()> {
    let (_tmp, store, old) = setup("s_a2_a1", Some("a2"))?;
    let new = chunk_with_trust("chunk-new", "s_a2_a1", Some("a1"));
    let _ = apply_supersede(&store, &old, new)?;

    let refreshed = store.get_chunk(&old.id)?.expect("old chunk persists");
    assert!(!refreshed.is_latest);
    assert_eq!(count_trust_guard_blocked(&store, "s_a2_a1"), 0);
    Ok(())
}

#[test]
fn test_a2_can_supersede_a2_via_apply_supersede() -> Result<()> {
    let (_tmp, store, old) = setup("s_a2_a2", Some("a2"))?;
    let new = chunk_with_trust("chunk-new", "s_a2_a2", Some("a2"));
    let _ = apply_supersede(&store, &old, new)?;

    let refreshed = store.get_chunk(&old.id)?.expect("old chunk persists");
    assert!(!refreshed.is_latest);
    assert_eq!(count_trust_guard_blocked(&store, "s_a2_a2"), 0);
    Ok(())
}

#[test]
fn test_a1_can_supersede_b_via_apply_supersede() -> Result<()> {
    let (_tmp, store, old) = setup("s_b_a1", Some("b"))?;
    let new = chunk_with_trust("chunk-new", "s_b_a1", Some("a1"));
    let _ = apply_supersede(&store, &old, new)?;

    let refreshed = store.get_chunk(&old.id)?.expect("old chunk persists");
    assert!(!refreshed.is_latest);
    assert_eq!(count_trust_guard_blocked(&store, "s_b_a1"), 0);
    Ok(())
}

// ─── apply_supersede: negative cases (truth table blocks) ────────────────────

#[test]
fn test_a2_cannot_supersede_a1_via_apply_supersede() -> Result<()> {
    let (_tmp, store, old) = setup("s_a1_a2", Some("a1"))?;
    let new = chunk_with_trust("chunk-new", "s_a1_a2", Some("a2"));
    let returned = apply_supersede(&store, &old, new)?;

    let refreshed = store.get_chunk(&old.id)?.expect("old chunk persists");
    assert!(refreshed.is_latest, "guard must keep old.is_latest=true");
    assert!(refreshed.superseded_by.is_none());
    // Returned new_chunk has no version-chain links when blocked.
    assert!(returned.supersedes_id.is_none());
    assert_eq!(count_trust_guard_blocked(&store, "s_a1_a2"), 1);
    Ok(())
}

#[test]
fn test_b_cannot_supersede_a1_via_apply_supersede() -> Result<()> {
    let (_tmp, store, old) = setup("s_a1_b", Some("a1"))?;
    let new = chunk_with_trust("chunk-new", "s_a1_b", Some("b"));
    let _ = apply_supersede(&store, &old, new)?;

    let refreshed = store.get_chunk(&old.id)?.expect("old chunk persists");
    assert!(refreshed.is_latest);
    assert_eq!(count_trust_guard_blocked(&store, "s_a1_b"), 1);
    Ok(())
}

#[test]
fn test_b_cannot_supersede_a2_via_apply_supersede() -> Result<()> {
    let (_tmp, store, old) = setup("s_a2_b", Some("a2"))?;
    let new = chunk_with_trust("chunk-new", "s_a2_b", Some("b"));
    let _ = apply_supersede(&store, &old, new)?;

    let refreshed = store.get_chunk(&old.id)?.expect("old chunk persists");
    assert!(refreshed.is_latest);
    assert_eq!(count_trust_guard_blocked(&store, "s_a2_b"), 1);
    Ok(())
}

// ─── try_supersede_with_guard (dream path) ───────────────────────────────────

#[test]
fn test_a2_cannot_supersede_a1_via_dream_helper() -> Result<()> {
    let (_tmp, store, old) = setup("s_dream_a1_a2", Some("a1"))?;
    let applied = try_supersede_with_guard(&store, &old, "chunk-new", Some("a2"), "dream", None)?;

    assert!(!applied, "dream A2->A1 must be blocked");
    let refreshed = store.get_chunk(&old.id)?.expect("old chunk persists");
    assert!(refreshed.is_latest);
    assert!(refreshed.superseded_by.is_none());
    assert_eq!(count_trust_guard_blocked(&store, "s_dream_a1_a2"), 1);
    Ok(())
}

#[test]
fn test_a1_can_supersede_a1_via_dream_helper() -> Result<()> {
    let (_tmp, store, old) = setup("s_dream_a1_a1", Some("a1"))?;
    let applied = try_supersede_with_guard(&store, &old, "chunk-new", Some("a1"), "dream", None)?;

    assert!(applied, "A1->A1 must succeed even via dream helper");
    let refreshed = store.get_chunk(&old.id)?.expect("old chunk persists");
    assert!(!refreshed.is_latest);
    assert_eq!(refreshed.superseded_by.as_deref(), Some("chunk-new"));
    assert_eq!(count_trust_guard_blocked(&store, "s_dream_a1_a1"), 0);
    Ok(())
}

// ─── audit log assertions (payload shape + presence/absence) ─────────────────

#[test]
fn test_blocked_supersede_writes_audit_entry() -> Result<()> {
    let (_tmp, store, old) = setup("s_audit_blocked", Some("a1"))?;
    let applied = try_supersede_with_guard(&store, &old, "chunk-new", Some("a2"), "dream", None)?;
    assert!(!applied);

    let events = audit::list(&store, "s_audit_blocked", 100)?.events;
    let blocked: Vec<_> = events
        .iter()
        .filter(|e| e.action == "trust_guard_blocked")
        .collect();
    assert_eq!(blocked.len(), 1, "exactly one trust_guard_blocked entry");
    let ev = blocked[0];
    assert_eq!(ev.actor_id, audit::SYSTEM_ACTOR_ID);
    assert_eq!(ev.details["op"], "trust_guard_blocked");
    assert_eq!(ev.details["old_chunk_id"], "chunk-old");
    assert_eq!(ev.details["old_trust"], "a1");
    assert_eq!(ev.details["new_chunk_id"], "chunk-new");
    assert_eq!(ev.details["new_trust"], "a2");
    assert_eq!(ev.details["context"], "dream");
    Ok(())
}

#[test]
fn test_allowed_supersede_writes_no_audit_entry() -> Result<()> {
    let (_tmp, store, old) = setup("s_audit_allowed", Some("a1"))?;
    let new = chunk_with_trust("chunk-new", "s_audit_allowed", Some("a1"));
    let _ = apply_supersede(&store, &old, new)?;

    let refreshed = store.get_chunk(&old.id)?.expect("old chunk persists");
    assert!(!refreshed.is_latest, "supersede must succeed");
    assert_eq!(count_trust_guard_blocked(&store, "s_audit_allowed"), 0);
    Ok(())
}

// ─── extra_old_mutator hook (cycle /40a) ─────────────────────────────────────

#[test]
fn test_extra_mutator_runs_atomically_on_apply() -> Result<()> {
    use loomem_core::storage::{ExtractionMeta, FactType};

    let (_tmp, store, mut old) = setup("s_mutator_apply", Some("a1"))?;
    // Seed extraction_meta on old chunk so the mutator has something to mutate.
    old.extraction_meta = Some(ExtractionMeta {
        fact_type: FactType::PreferenceOrDecision,
        subject: Some("subject".to_string()),
        event_date: None,
        event_date_context: None,
        supersedes: None,
        superseded_by: None,
        confidence: 0.9,
        extracted_from: None,
        extraction_model: None,
        original_content: None,
        topic: None,
    });
    store.store_chunk(&old)?;

    // A1->A1 supersede with a mutator that lays down dream-specific bookkeeping.
    let now_marker: u64 = now_secs();
    let applied = try_supersede_with_guard(
        &store,
        &old,
        "chunk-new",
        Some("a1"),
        "dream",
        Some(&|c: &mut Chunk| {
            c.valid_until = Some(now_marker);
            if let Some(ref mut meta) = c.extraction_meta {
                meta.superseded_by = Some("dream-consolidated".to_string());
            }
        }),
    )?;
    assert!(applied, "A1->A1 must apply");

    let refreshed = store.get_chunk(&old.id)?.expect("old chunk persists");
    assert!(!refreshed.is_latest, "is_latest flipped by helper");
    assert_eq!(refreshed.superseded_by.as_deref(), Some("chunk-new"));
    assert_eq!(
        refreshed.valid_until,
        Some(now_marker),
        "mutator's valid_until landed in the same store_chunk"
    );
    assert_eq!(
        refreshed
            .extraction_meta
            .as_ref()
            .and_then(|m| m.superseded_by.as_deref()),
        Some("dream-consolidated"),
        "mutator's extraction_meta.superseded_by landed in the same store_chunk"
    );
    assert_eq!(count_trust_guard_blocked(&store, "s_mutator_apply"), 0);
    Ok(())
}

#[test]
fn test_extra_mutator_skipped_on_guard_block() -> Result<()> {
    let (_tmp, store, old) = setup("s_mutator_block", Some("a1"))?;

    // A2->A1 is blocked. Mutator must NOT run.
    let mutator_called = std::cell::Cell::new(false);
    let applied = try_supersede_with_guard(
        &store,
        &old,
        "chunk-new",
        Some("a2"),
        "dream",
        Some(&|c: &mut Chunk| {
            mutator_called.set(true);
            c.valid_until = Some(42);
        }),
    )?;

    assert!(!applied, "A2->A1 must be blocked");
    assert!(!mutator_called.get(), "mutator must not run on guard block");
    let refreshed = store.get_chunk(&old.id)?.expect("old chunk persists");
    assert!(refreshed.is_latest, "guard kept is_latest=true");
    assert!(refreshed.valid_until.is_none(), "mutator did not write");
    assert_eq!(count_trust_guard_blocked(&store, "s_mutator_block"), 1);
    Ok(())
}

// ─── dream pure-logic e2e (cycle /40a, AC-3 follow-up) ───────────────────────

/// End-to-end exercise of the dream supersede path through the public
/// `apply_dream_response_for_subject` helper. This is the storage-mutating
/// portion of `dream::dream_run` after the LLM call returns; the LLM HTTP
/// call itself has no in-tree mock infrastructure (hard-coded
/// `https://api.openai.com/v1/chat/completions`), so we feed the helper a
/// synthesised `LlmDreamResponse` directly. The trust-guard semantics
/// exercised here are byte-identical to what `dream_run` does on the same
/// LLM payload.
/// cycle /46: apply_dream_response_for_subject is now async (adds Tantivy write).
/// Test updated from #[test] to #[tokio::test] as a necessary consequence of
/// the documented architectural change in dream.rs (CS2 callsite migration).
#[tokio::test]
async fn test_dream_run_e2e_blocks_a1_supersede() -> Result<()> {
    use loomem_core::dream::{
        apply_dream_response_for_subject, DreamApplyContext, LlmContradiction, LlmDreamResponse,
    };
    use loomem_core::storage::{ExtractionMeta, FactType};

    // Setup: A1 chunk seeded with subject + content matching the brief scenario.
    let stream = "s_dream_e2e";
    let (_tmp, store, mut a1) = setup(stream, Some("a1"))?;
    a1.id = "a1-cardiologist".to_string();
    a1.content = "Cardiologist suspects sitting at work.".to_string();
    a1.extraction_meta = Some(ExtractionMeta {
        fact_type: FactType::PreferenceOrDecision,
        subject: Some("back pain".to_string()),
        event_date: None,
        event_date_context: None,
        supersedes: None,
        superseded_by: None,
        confidence: 0.9,
        extracted_from: None,
        extraction_model: None,
        original_content: None,
        topic: None,
    });
    store.store_chunk(&a1)?;

    // Tantivy in tempdir for the dream Tantivy write (cycle /46 CS2)
    let tantivy_tmp = TempDir::new()?;
    let tantivy_raw = TantivyIndex::open(tantivy_tmp.path().join("tantivy"), &tantivy_config())?;
    let tantivy = tokio::sync::Mutex::new(tantivy_raw);

    // Synthesise the LLM payload that `dream_run`'s parser would have produced.
    let dream_resp = LlmDreamResponse {
        merged_fact: "Asymmetric pedalling causes back pain.".to_string(),
        fact_type: Some("preference_or_decision".to_string()),
        fact_date: None,
        contradictions: vec![LlmContradiction {
            old_uuid: a1.id.clone(),
            reason: "PT diagnosis supersedes initial cardiologist guess".to_string(),
        }],
        confidence: 0.9,
    };

    let chunks_in_group = vec![a1.clone()];
    let (facts_merged, contradictions_resolved) = apply_dream_response_for_subject(
        &store,
        &tantivy,
        DreamApplyContext {
            stream,
            subject: "back pain",
            chunks: &chunks_in_group,
            model: "gpt-4.1-mini",
            intent_log: None,
        },
        dream_resp,
    )
    .await?;

    // Assertions per brief AC-3 follow-up:
    // 1. A1 chunk is untouched (guard blocked the supersede).
    let refreshed_a1 = store.get_chunk(&a1.id)?.expect("a1 persists");
    assert!(
        refreshed_a1.is_latest,
        "A1 must remain is_latest=true (dream-derived A2 cannot supersede)"
    );
    assert!(refreshed_a1.superseded_by.is_none());
    assert!(
        refreshed_a1.valid_until.is_none(),
        "mutator must not have run on the blocked path"
    );

    // 2. Audit log has exactly one trust_guard_blocked entry with context="dream".
    let blocked = count_trust_guard_blocked(&store, stream);
    assert_eq!(blocked, 1, "expected one trust_guard_blocked audit entry");
    let events = audit::list(&store, stream, 100)?.events;
    let ev = events
        .iter()
        .find(|e| e.action == "trust_guard_blocked")
        .expect("blocked entry");
    assert_eq!(ev.details["context"], "dream");
    assert_eq!(ev.details["new_trust"], "a2");
    assert_eq!(ev.details["old_chunk_id"], a1.id);

    // 3. contradictions_resolved counter is zero (block does not count).
    assert_eq!(contradictions_resolved, 0);

    // 4. The new dream chunk was still written separately as A2 / is_latest.
    assert_eq!(facts_merged, 1);
    let dream_chunks: Vec<_> = store
        .prefix_scan(b"chunk:L1:")
        .filter_map(|(_k, v)| serde_json::from_slice::<loomem_core::storage::Chunk>(&v).ok())
        .filter(|c| c.stream == stream)
        .collect();
    assert_eq!(dream_chunks.len(), 1, "exactly one dream-derived L1 chunk");
    let dream = &dream_chunks[0];
    assert!(dream.id.starts_with("dream:"));
    assert!(dream.is_latest);
    assert_eq!(dream.trust_level.as_deref(), Some("a2"));
    assert_eq!(dream.content, "Asymmetric pedalling causes back pain.");
    assert!(
        dream.supersedes_id.is_none(),
        "blocked supersede must not produce a version-chain link on the new chunk"
    );

    Ok(())
}
