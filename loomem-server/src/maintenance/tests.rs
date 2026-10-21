use super::*;
use loomem_core::{config::RocksDbConfig, storage::Chunk};

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
        provenance_role: loomem_core::storage::ProvenanceRole::Claim,
        fragment: None,
    }
}

fn store(path: &std::path::Path) -> RocksDbStore {
    RocksDbStore::open(
        path.join("rocks"),
        &RocksDbConfig {
            max_open_files: 100,
            compression: "lz4".into(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        },
    )
    .unwrap()
}

#[test]
fn drift_catches_orphan_only_index_and_preserves_normal_ratios() {
    assert_eq!(index_drift_pct(0, 0), 0.0);
    assert_eq!(index_drift_pct(0, 8), 100.0);
    assert_eq!(index_drift_pct(10, 10), 0.0);
    assert_eq!(index_drift_pct(10, 8), 20.0);
    assert_eq!(index_drift_pct(10, 12), 20.0);
    assert_eq!(index_drift_pct(10, 0), 100.0);
}

#[test]
fn startup_auto_rebuild_removes_orphans_when_primary_is_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let store = store(tmp.path());
    let config = TantivyConfig {
        enabled: true,
        heap_size_mb: 50,
        drift_warn_pct: 5.0,
        auto_rebuild_on_drift: true,
    };
    let mut index = TantivyIndex::open(tmp.path().join("index"), &config).unwrap();
    index
        .upsert_document(loomem_core::tantivy_index::TextDocument {
            id: "orphan".into(),
            content: "orphaned needleword".into(),
            user_id: String::new(),
            app_id: String::new(),
            level: 0,
            stream: "alpha".into(),
            timestamp: 1_700_000_000,
            entities: None,
            relations: None,
            event_date: None,
            source_agent: None,
        })
        .unwrap();
    index.commit().unwrap();
    assert_eq!(index.count().unwrap(), 1);
    check_index_drift(&config, &store, &mut index).unwrap();
    assert_eq!(index.count().unwrap(), 0);
    assert!(index.search("needleword", 10).unwrap().is_empty());
}

#[tokio::test]
async fn reembed_skips_deleted_source_before_inference() {
    let tmp = tempfile::tempdir().unwrap();
    let store = store(tmp.path());
    let mut chunk = make_chunk("gone", "alpha", "old content");
    chunk.deleted_at = Some(1_700_000_001);
    store.store_chunk(&chunk).unwrap();
    let published = reembed_chunk(&store, "gone", |_| async {
        panic!("deleted source must not invoke inference");
    })
    .await
    .unwrap();
    assert!(!published);
}

#[tokio::test]
async fn reembed_rejects_source_deleted_during_inference() {
    let tmp = tempfile::tempdir().unwrap();
    let store = store(tmp.path());
    store
        .store_chunk(&make_chunk("gone", "alpha", "old content"))
        .unwrap();
    let published = reembed_chunk(&store, "gone", |_| async {
        store.hard_delete_by_id("gone").unwrap();
        Ok(vec![1.0, 0.0])
    })
    .await
    .unwrap();
    assert!(!published);
    assert!(store.get_embedding("gone").unwrap().is_none());
}

#[tokio::test]
async fn reembed_rejects_changed_source_and_publishes_current_source() {
    let tmp = tempfile::tempdir().unwrap();
    let store = store(tmp.path());
    let mut chunk = make_chunk("changed", "alpha", "old content");
    store.store_chunk(&chunk).unwrap();
    let published = reembed_chunk(&store, "changed", |_| async {
        chunk.content = "new content".into();
        store.store_chunk(&chunk).unwrap();
        Ok(vec![1.0, 0.0])
    })
    .await
    .unwrap();
    assert!(!published);
    assert!(store.get_embedding("changed").unwrap().is_none());
    let published = reembed_chunk(&store, "changed", |content| async move {
        assert_eq!(content, "new content");
        Ok(vec![0.0, 1.0])
    })
    .await
    .unwrap();
    assert!(published);
    assert_eq!(
        store.get_embedding("changed").unwrap().unwrap(),
        vec![0.0, 1.0]
    );
}

#[tokio::test]
async fn dimension_migration_refuses_retained_tombstone_and_preserves_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let store = store(tmp.path());
    let chunk = make_chunk("retained", "alpha", "old content");
    store.store_chunk(&chunk).unwrap();
    store.store_embedding(&chunk.id, vec![1.0, 0.0]).unwrap();
    store.set_embedding_dim(2).unwrap();
    store.delete_by_id(&chunk.id).unwrap();
    let existing = store.get_all_embeddings().unwrap();
    assert!(validate_reembed_sources(&store, &existing, 3).is_err());
    assert!(!reembed_chunk(&store, &chunk.id, |_| async {
        panic!("tombstone must not be embedded");
    })
    .await
    .unwrap());
    assert!(record_reembedded_dimension(&store, 3).is_err());
    assert_eq!(store.stored_embedding_dim().unwrap(), Some(2));
    assert_eq!(store.get_embedding(&chunk.id).unwrap().unwrap().len(), 2);
    // Keeping the same dimension is valid; no recoverable vector is discarded.
    validate_reembed_sources(&store, &existing, 2).unwrap();
    record_reembedded_dimension(&store, 2).unwrap();
}

#[test]
fn dimension_metadata_requires_every_vector_to_match() {
    let tmp = tempfile::tempdir().unwrap();
    let store = store(tmp.path());
    store.store_embedding("orphan", vec![1.0, 0.0]).unwrap();
    store.set_embedding_dim(2).unwrap();
    assert!(validate_reembed_sources(&store, &store.get_all_embeddings().unwrap(), 3).is_err());
    assert!(record_reembedded_dimension(&store, 3).is_err());
    assert_eq!(store.stored_embedding_dim().unwrap(), Some(2));
    store
        .store_embedding("orphan", vec![1.0, 0.0, 0.0])
        .unwrap();
    record_reembedded_dimension(&store, 3).unwrap();
    assert_eq!(store.stored_embedding_dim().unwrap(), Some(3));
}
