use crate::handlers::delete::{delete_memory_fully, delete_memory_fully_at, Clock};

struct FixedClock;

impl Clock for FixedClock {
    fn timestamp(&self) -> anyhow::Result<u64> {
        Ok(4242)
    }
}
use loomem_core::{
    config::{RocksDbConfig, TantivyConfig},
    graph::GraphStore,
    source_tag::SourceTag,
    storage::{Chunk, ProvenanceRole, RocksDbStore},
    tantivy_index::{TantivyIndex, TextDocument},
};
use std::sync::Arc;
use tokio::sync::Mutex;

fn chunk(id: &str) -> Chunk {
    serde_json::from_value(serde_json::json!({"id":id,"content":"synthetic source","stream":"test","level":0,"score":1.0,"timestamp":1,"consolidated":false,"dormant":false,"in_progress":false})).unwrap()
}
#[tokio::test]
async fn source_cleanup_continues_after_child_graph_failure_and_retries_tombstones() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        RocksDbStore::open(
            temp.path().join("db"),
            &RocksDbConfig {
                max_open_files: 100,
                compression: "lz4".into(),
                write_buffer_size: 4 * 1024 * 1024,
                max_write_buffer_number: 2,
            },
        )
        .unwrap(),
    );
    let index = Mutex::new(
        TantivyIndex::open(
            temp.path().join("fts"),
            &TantivyConfig {
                enabled: true,
                heap_size_mb: 50,
                drift_warn_pct: 5.0,
                auto_rebuild_on_drift: false,
            },
        )
        .unwrap(),
    );
    let graph = GraphStore::new(Arc::clone(&store));
    let mut root = chunk(&uuid::Uuid::new_v4().to_string());
    root.provenance_role = ProvenanceRole::Evidence;
    root.persistent = true;
    root.consolidated = true;
    root.trust_level = Some("a2".into());
    root.source = Some(SourceTag::from_agent("mcp-ingest-source"));
    root.metadata = Some(serde_json::json!({"_loomem_ingest":{"version":1,"kind":"source"}}));
    let mut child = chunk("legacy-derived");
    child.source_ids = Some(vec![root.id.clone()]);
    store.store_chunk(&root).unwrap();
    store.store_chunk(&child).unwrap();
    for c in [&root, &child] {
        store.store_embedding(&c.id, vec![1.0]).unwrap();
        index
            .lock()
            .await
            .index_document(TextDocument {
                id: c.id.clone(),
                content: c.content.clone(),
                user_id: "test".into(),
                app_id: "default".into(),
                level: 0,
                timestamp: 1,
                stream: c.stream.clone(),
                entities: None,
                relations: None,
                event_date: None,
                source_agent: None,
            })
            .unwrap();
    }
    index.lock().await.commit().unwrap();
    store
        .put(format!("graph:chunk:{}", child.id).as_bytes(), b"not-json")
        .unwrap();
    let first = delete_memory_fully_at(&store, &index, &graph, &root.id, &FixedClock)
        .await
        .unwrap();
    assert!(first.store_deleted);
    assert!(first.tantivy.is_ok());
    assert!(first.embedding.is_ok());
    assert!(first.graph.is_err());
    assert!(!first.all_ok());
    for c in [&root, &child] {
        assert_eq!(
            store.get_chunk(&c.id).unwrap().unwrap().deleted_at,
            Some(4242)
        );
        assert!(store.get_embedding(&c.id).unwrap().is_none());
    }
    store
        .put(format!("graph:chunk:{}", child.id).as_bytes(), b"[]")
        .unwrap();
    let retry = delete_memory_fully(&store, &index, &graph, &root.id)
        .await
        .unwrap();
    assert!(retry.store_deleted);
    assert!(retry.all_ok());
}

#[tokio::test]
async fn source_delete_cleans_fragment_from_store_index_and_embedding() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        RocksDbStore::open(
            temp.path().join("db"),
            &RocksDbConfig {
                max_open_files: 100,
                compression: "lz4".into(),
                write_buffer_size: 4 * 1024 * 1024,
                max_write_buffer_number: 2,
            },
        )
        .unwrap(),
    );
    let index = Mutex::new(
        TantivyIndex::open(
            temp.path().join("fts"),
            &TantivyConfig {
                enabled: true,
                heap_size_mb: 50,
                drift_warn_pct: 5.0,
                auto_rebuild_on_drift: false,
            },
        )
        .unwrap(),
    );
    let graph = GraphStore::new(Arc::clone(&store));
    let mut root = chunk(&uuid::Uuid::new_v4().to_string());
    root.provenance_role = ProvenanceRole::Evidence;
    root.persistent = true;
    root.consolidated = true;
    root.trust_level = Some("a2".into());
    root.source = Some(SourceTag::from_agent("mcp-ingest-source"));
    root.metadata = Some(serde_json::json!({"_loomem_ingest":{"version":1,"kind":"source"}}));
    let mut template = chunk("template");
    template.trust_level = Some("a2".into());
    template.source = Some(SourceTag::from_agent("mcp-ingest"));
    template.metadata = Some(serde_json::json!({"_loomem_ingest":{"version":1,"kind":"fragment"}}));
    let fragment = loomem_core::fragments::fragment_chunk(
        &loomem_core::fragments::SourceFragment {
            id: loomem_core::fragments::fragment_id(&root.id, 0),
            source_id: root.id.clone(),
            index: 0,
            byte_start: 0,
            byte_end: root.content.len(),
            turn_index: 0,
            role: None,
            conversation_date: None,
            text: root.content.clone(),
        },
        &template,
    );
    assert!(loomem_core::storage::is_ingest_fragment(&fragment));
    store.store_chunk(&root).unwrap();
    store.store_chunk(&fragment).unwrap();
    for c in [&root, &fragment] {
        store.store_embedding(&c.id, vec![1.0]).unwrap();
        index
            .lock()
            .await
            .index_document(TextDocument {
                id: c.id.clone(),
                content: c.content.clone(),
                user_id: "test".into(),
                app_id: "default".into(),
                level: 0,
                timestamp: 1,
                stream: c.stream.clone(),
                entities: None,
                relations: None,
                event_date: None,
                source_agent: None,
            })
            .unwrap();
    }
    index.lock().await.commit().unwrap();
    let outcome = delete_memory_fully_at(&store, &index, &graph, &root.id, &FixedClock)
        .await
        .unwrap();
    assert!(outcome.store_deleted);
    assert!(outcome.all_ok());
    for c in [&root, &fragment] {
        assert_eq!(
            store.get_chunk(&c.id).unwrap().unwrap().deleted_at,
            Some(4242)
        );
        assert!(store.get_embedding(&c.id).unwrap().is_none());
    }
    let hits = index
        .lock()
        .await
        .search_with_stream("synthetic source", "test", 10)
        .unwrap();
    assert!(hits.is_empty());
}
