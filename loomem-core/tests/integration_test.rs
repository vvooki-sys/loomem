//! Integration tests for the store → search → consolidation-eligibility pipeline.
//!
//! These tests use real RocksDB and Tantivy instances in temp directories.
//! No LLM API calls are made — consolidation is tested at the eligibility-scan
//! level only (the LLM compression step requires an external API key).

use anyhow::Result;
use loomem_core::config::{RocksDbConfig, TantivyConfig};
use loomem_core::{storage::Chunk, RocksDbStore, TantivyIndex, TextDocument};
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

fn make_chunk(id: &str, content: &str, stream: &str, level: i32, age_secs: u64) -> Chunk {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time is after epoch")
        .as_secs();

    Chunk {
        id: id.to_string(),
        content: content.to_string(),
        stream: stream.to_string(),
        level,
        score: 1.0,
        timestamp: now.saturating_sub(age_secs),
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

fn make_doc(chunk: &Chunk) -> TextDocument {
    TextDocument {
        id: chunk.id.clone(),
        content: chunk.content.clone(),
        user_id: "test".to_string(),
        app_id: "test".to_string(),
        level: chunk.level,
        timestamp: chunk.timestamp as i64,
        stream: chunk.stream.clone(),
        entities: None,
        relations: None,
        event_date: None,
        source_agent: chunk.source.as_ref().map(|s| s.agent.clone()),
    }
}

// ─── test 1: store → search (BM25) ──────────────────────────────────────────

#[test]
fn test_store_and_search_bm25() -> Result<()> {
    let tmp = TempDir::new().expect("create temp dir");
    let store = RocksDbStore::open(tmp.path().join("rocks"), &rocksdb_config())?;
    let mut tantivy = TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config())?;

    // Store two chunks
    let c1 = make_chunk("id-1", "The quick brown fox jumps", "100", 0, 0);
    let c2 = make_chunk("id-2", "Lazy dog resting in the sun", "100", 0, 0);

    store.store_chunk(&c1)?;
    store.store_chunk(&c2)?;

    tantivy.index_document(make_doc(&c1))?;
    tantivy.index_document(make_doc(&c2))?;
    tantivy.commit()?;

    // Search for "fox"
    let results = tantivy.search("fox", 10)?;
    assert!(
        !results.is_empty(),
        "Expected at least one result for 'fox'"
    );
    assert_eq!(results[0].id, "id-1", "Top result should be the fox chunk");

    // Search for "dog"
    let results = tantivy.search("dog", 10)?;
    assert!(
        !results.is_empty(),
        "Expected at least one result for 'dog'"
    );
    assert_eq!(results[0].id, "id-2", "Top result should be the dog chunk");

    Ok(())
}

// ─── test 2: stream-scoped search ───────────────────────────────────────────

#[test]
fn test_stream_scoped_search() -> Result<()> {
    let tmp = TempDir::new().expect("create temp dir");
    let store = RocksDbStore::open(tmp.path().join("rocks"), &rocksdb_config())?;
    let mut tantivy = TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config())?;

    // Two chunks with the same keyword in different streams
    let c1 = make_chunk("id-a", "project deadline approaching fast", "100", 0, 0);
    let c2 = make_chunk("id-b", "project kickoff meeting scheduled", "200", 0, 0);

    store.store_chunk(&c1)?;
    store.store_chunk(&c2)?;

    tantivy.index_document(make_doc(&c1))?;
    tantivy.index_document(make_doc(&c2))?;
    tantivy.commit()?;

    // Search scoped to stream "100" — should not return stream "200" chunk
    let results = tantivy.search_with_stream("project", "100", 10)?;
    assert!(!results.is_empty(), "Expected result in stream 100");
    assert!(
        results.iter().all(|r| r.stream == "100"),
        "All results must belong to stream 100"
    );

    // Search scoped to stream "200"
    let results_200 = tantivy.search_with_stream("project", "200", 10)?;
    assert!(!results_200.is_empty(), "Expected result in stream 200");
    assert!(
        results_200.iter().all(|r| r.stream == "200"),
        "All results must belong to stream 200"
    );

    Ok(())
}

// ─── test 3: entity-tagged search ───────────────────────────────────────────

#[test]
fn test_entity_tagged_search() -> Result<()> {
    let tmp = TempDir::new().expect("create temp dir");
    let store = RocksDbStore::open(tmp.path().join("rocks"), &rocksdb_config())?;
    let mut tantivy = TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config())?;

    // Chunk with entity tag
    let c1 = make_chunk(
        "id-entity",
        "Anna discussed the budget with the Acme team",
        "100",
        0,
        0,
    );
    let c2 = make_chunk(
        "id-no-entity",
        "The weather was nice yesterday",
        "100",
        0,
        0,
    );

    store.store_chunk(&c1)?;
    store.store_chunk(&c2)?;

    let mut doc1 = make_doc(&c1);
    doc1.entities = Some("Anna,Acme".to_string());

    tantivy.index_document(doc1)?;
    tantivy.index_document(make_doc(&c2))?;
    tantivy.commit()?;

    // Entity search — should prefer the tagged chunk
    let results = tantivy.search_with_entity("budget", "Anna", 10)?;
    assert!(!results.is_empty(), "Expected results for entity search");
    assert_eq!(
        results[0].id, "id-entity",
        "Entity-tagged chunk should rank first"
    );

    Ok(())
}

// ─── test 4: consolidation eligibility scan ──────────────────────────────────

#[test]
fn test_consolidation_eligibility_scan() -> Result<()> {
    let tmp = TempDir::new().expect("create temp dir");
    let store = RocksDbStore::open(tmp.path().join("rocks"), &rocksdb_config())?;

    // 3 old unconsolidated chunks (1h old) + 1 fresh chunk + 1 already consolidated
    let old_1 = make_chunk("old-1", "old content alpha", "100", 0, 3700);
    let old_2 = make_chunk("old-2", "old content beta", "100", 0, 3700);
    let old_3 = make_chunk("old-3", "old content gamma", "200", 0, 3700); // different stream
    let fresh = make_chunk("fresh-1", "fresh content", "100", 0, 60); // 1 minute old
    let mut consolidated = make_chunk("cons-1", "already consolidated", "100", 0, 3700);
    consolidated.consolidated = true;

    store.store_chunk(&old_1)?;
    store.store_chunk(&old_2)?;
    store.store_chunk(&old_3)?;
    store.store_chunk(&fresh)?;
    store.store_chunk(&consolidated)?;

    // Scan for chunks older than 1h (3600s)
    let eligible = store.scan_l0_unconsolidated(3600, 100)?;

    assert_eq!(eligible.len(), 3, "Should find 3 old unconsolidated chunks");
    assert!(
        eligible.iter().all(|c| !c.consolidated),
        "All eligible chunks must be unconsolidated"
    );
    assert!(
        eligible.iter().all(|c| !c.in_progress),
        "All eligible chunks must not be in_progress"
    );

    // Fresh chunk should NOT be eligible
    assert!(
        !eligible.iter().any(|c| c.id == "fresh-1"),
        "Fresh chunk should not be eligible"
    );

    // Consolidated chunk should NOT be eligible
    assert!(
        !eligible.iter().any(|c| c.id == "cons-1"),
        "Already-consolidated chunk should not be eligible"
    );

    Ok(())
}

// ─── test 5: orphan recovery on restart ──────────────────────────────────────

#[test]
fn test_orphan_recovery() -> Result<()> {
    let tmp = TempDir::new().expect("create temp dir");
    let store = RocksDbStore::open(tmp.path().join("rocks"), &rocksdb_config())?;

    // Simulate a crash mid-consolidation: 2 chunks marked in_progress
    let mut c1 = make_chunk("orphan-a", "orphaned alpha", "100", 0, 3700);
    let mut c2 = make_chunk("orphan-b", "orphaned beta", "100", 0, 3700);
    c1.in_progress = true;
    c2.in_progress = true;
    let c3 = make_chunk("normal", "normal chunk", "100", 0, 0);

    store.store_chunk(&c1)?;
    store.store_chunk(&c2)?;
    store.store_chunk(&c3)?;

    // Recovery should clear in_progress flag
    let recovered = store.recover_orphaned_chunks()?;
    assert_eq!(recovered, 2, "Should recover exactly 2 orphaned chunks");

    // After recovery, orphans must no longer block consolidation
    let eligible = store.scan_l0_unconsolidated(3600, 100)?;
    assert!(
        eligible.iter().any(|c| c.id == "orphan-a"),
        "Recovered orphan-a should now be eligible"
    );
    assert!(
        eligible.iter().any(|c| c.id == "orphan-b"),
        "Recovered orphan-b should now be eligible"
    );

    Ok(())
}

// ─── test 6: full round-trip: store → BM25 → score boost ─────────────────────

#[test]
fn test_round_trip_store_search_boost() -> Result<()> {
    let tmp = TempDir::new().expect("create temp dir");
    let store = RocksDbStore::open(tmp.path().join("rocks"), &rocksdb_config())?;
    let mut tantivy = TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config())?;

    let chunk = make_chunk("rt-1", "Rust async programming patterns", "100", 1, 0);
    store.store_chunk(&chunk)?;
    tantivy.index_document(make_doc(&chunk))?;
    tantivy.commit()?;

    // Search and verify found
    let results = tantivy.search("async programming", 5)?;
    assert!(!results.is_empty(), "Should find chunk via BM25");
    let found_id = &results[0].id;
    assert_eq!(found_id, "rt-1");

    // Simulate access boost (as search_handler does)
    store.boost_score(found_id)?;

    // Verify score was reset to 1.0
    let updated = store
        .get_chunk(found_id)?
        .expect("chunk should still exist");
    assert_eq!(updated.score, 1.0, "Access boost should reset score to 1.0");

    // Verify importance boost
    store.boost_importance(found_id)?;
    let important = store
        .get_chunk(found_id)?
        .expect("chunk should still exist");
    assert_eq!(
        important.importance,
        Some(1.5),
        "Importance boost should set 1.5"
    );

    Ok(())
}
