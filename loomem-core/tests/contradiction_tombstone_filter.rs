//! Integration tests for cycle/80: contradiction.rs paths must skip
//! tombstoned chunks even when their embedding is still in CF_EMBEDDINGS.
//!
//! Background: /78 fixed `delete_memory_fully` to hard-delete the embedding,
//! but legacy zombie embeddings (from pre-/78 deletes within the ~30-day
//! hard-purge window) can still sit in the embedding store. Three sites
//! in `contradiction.rs` previously trusted `chunk.stream + is_latest` as
//! sufficient candidate filter:
//!
//! - `find_candidates` — feeds LLM `classify_relation` with dead candidates.
//! - `dedup_check` — **silent write loss**: matches tombstone, bumps
//!   `access_count` on the tombstone, returns `Duplicate(id)`, caller
//!   skips storing the new chunk. New content disappears.
//! - `detect_contradiction` — feeds LLM with dead candidates; "refinement"
//!   classification could attach `superseded_by` pointer to a tombstone.
//!
//! These tests construct that exact zombie-embedding state and verify the
//! filter rejects it.

use anyhow::Result;
use loomem_core::config::{ContradictionConfig, RocksDbConfig};
use loomem_core::contradiction::{dedup_check, find_candidates, DedupResult};
use loomem_core::storage::Chunk;
use loomem_core::RocksDbStore;
use tempfile::TempDir;

fn rocksdb_config() -> RocksDbConfig {
    RocksDbConfig {
        max_open_files: 50,
        compression: "lz4".to_string(),
        write_buffer_size: 4 * 1024 * 1024,
        max_write_buffer_number: 2,
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn live_chunk(id: &str, stream: &str, content: &str) -> Chunk {
    Chunk {
        id: id.to_string(),
        content: content.to_string(),
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
        trust_level: None,
        ingester_user_id: None,

        alpha: 1.0,
        beta: 1.0,
        harmful_count: 0,
        n_ratings: 0,
        last_rated_at: None,
    }
}

/// Seed a chunk + embedding, then soft-delete the chunk WITHOUT removing
/// the embedding — simulates pre-/78 zombie state OR a chunk inside the
/// 30-day post-/78 hard-purge window. Returns the store + chunk id.
fn setup_zombie(
    stream: &str,
    content: &str,
    embedding: Vec<f32>,
) -> Result<(TempDir, RocksDbStore, String)> {
    let temp = TempDir::new()?;
    let store = RocksDbStore::open(temp.path().join("rocks"), &rocksdb_config())?;
    let id = "zombie-chunk-1".to_string();

    let chunk = live_chunk(&id, stream, content);
    store.store_chunk(&chunk)?;
    store.store_embedding(&id, embedding)?;

    // Soft-delete the chunk (sets deleted_at) but DELIBERATELY leave the
    // embedding in CF_EMBEDDINGS — this is the zombie state we're guarding
    // against.
    store.delete_by_id(&id)?;

    let after = store
        .get_chunk(&id)?
        .expect("chunk persists post soft-delete");
    assert!(after.deleted_at.is_some(), "deleted_at should be set");
    assert!(
        store.get_embedding(&id)?.is_some(),
        "test setup must leave the zombie embedding in place"
    );

    Ok((temp, store, id))
}

#[test]
fn test_find_candidates_skips_tombstoned_chunk_with_high_similarity() -> Result<()> {
    let stream = "s_zombie_find";
    let new_emb = vec![0.5f32, 0.5, 0.5];
    // Identical embedding → cosine similarity 1.0, well above any threshold.
    let (_tmp, store, _id) = setup_zombie(stream, "old preference", new_emb.clone())?;

    let config = ContradictionConfig::default();
    let candidates = find_candidates(&store, &new_emb, stream, &config)?;

    assert!(
        candidates.is_empty(),
        "find_candidates must skip tombstoned chunks (got {} candidates)",
        candidates.len()
    );
    Ok(())
}

#[test]
fn test_dedup_check_does_not_match_tombstoned_chunk() -> Result<()> {
    // The silent-write-loss case: pre-fix, a delete-then-similar-store
    // sequence would return Duplicate(tombstone_id) instead of New,
    // bumping the tombstone's access_count and dropping the new content.
    let stream = "s_zombie_dedup";
    let new_emb = vec![0.5f32, 0.5, 0.5];
    let (_tmp, store, zombie_id) = setup_zombie(stream, "I prefer dark mode", new_emb.clone())?;

    let result = dedup_check(&store, &new_emb, stream, None, 0.9)?;

    match result {
        DedupResult::New => {} // expected
        DedupResult::Duplicate(matched_id) => panic!(
            "dedup_check matched a tombstoned chunk ({matched_id}) — silent-write-loss bug. \
             Expected DedupResult::New so caller stores the new chunk."
        ),
    }

    // Side-effect check: the tombstone must NOT have been bumped.
    let zombie_after = store.get_chunk(&zombie_id)?.expect("zombie persists");
    assert_eq!(
        zombie_after.access_count, 0,
        "tombstone access_count must not be bumped by dedup_check (was {})",
        zombie_after.access_count
    );
    assert!(
        zombie_after.deleted_at.is_some(),
        "tombstone must remain deleted"
    );
    Ok(())
}

#[test]
fn test_dedup_check_still_matches_live_chunk_with_same_content() -> Result<()> {
    // Sanity check that the new filter doesn't break the happy path:
    // a LIVE chunk with similar embedding must still be matched as Duplicate.
    let temp = TempDir::new()?;
    let store = RocksDbStore::open(temp.path().join("rocks"), &rocksdb_config())?;
    let stream = "s_live_dedup";

    let live = live_chunk("live-1", stream, "I prefer dark mode");
    store.store_chunk(&live)?;
    store.store_embedding("live-1", vec![0.5f32, 0.5, 0.5])?;

    let new_emb = vec![0.5f32, 0.5, 0.5]; // identical → cosine 1.0
    let result = dedup_check(&store, &new_emb, stream, None, 0.9)?;

    match result {
        DedupResult::Duplicate(id) => assert_eq!(id, "live-1"),
        DedupResult::New => panic!("dedup_check missed a live duplicate — filter is over-strict"),
    }
    Ok(())
}

#[test]
fn test_find_candidates_still_returns_live_similar_chunk() -> Result<()> {
    // Sanity check on the find_candidates path.
    let temp = TempDir::new()?;
    let store = RocksDbStore::open(temp.path().join("rocks"), &rocksdb_config())?;
    let stream = "s_live_find";

    let live = live_chunk("live-1", stream, "old preference");
    store.store_chunk(&live)?;
    store.store_embedding("live-1", vec![0.5f32, 0.5, 0.5])?;

    let new_emb = vec![0.5f32, 0.5, 0.5];
    let config = ContradictionConfig::default();
    let candidates = find_candidates(&store, &new_emb, stream, &config)?;

    assert_eq!(candidates.len(), 1, "live chunk must be returned");
    assert_eq!(candidates[0].chunk.id, "live-1");
    Ok(())
}
