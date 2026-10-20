use super::*;
use crate::{config::RocksDbConfig, pii_filter::PiiConfig, storage::RocksDbStore};
use std::sync::Arc;

fn setup(encrypted: bool) -> (tempfile::TempDir, Arc<RocksDbStore>, GraphStore, PiiFilter) {
    let temp = tempfile::tempdir().unwrap();
    let store = RocksDbStore::open(
        temp.path(),
        &RocksDbConfig {
            max_open_files: 100,
            compression: "none".into(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        },
    )
    .unwrap();
    let store = if encrypted {
        let provider = Arc::new(crate::MasterKeyEnvProvider::new([7; 32], store.db_arc()));
        store.with_encryption_provider(provider)
    } else {
        store
    };
    let store = Arc::new(store);
    let graph = GraphStore::new(store.clone());
    (
        temp,
        store,
        graph,
        PiiFilter::new(PiiConfig::default()).unwrap(),
    )
}

fn chunk(store: &RocksDbStore, content: &str) -> Chunk {
    let chunk: Chunk =
        serde_json::from_value(serde_json::json!({"id":uuid::Uuid::new_v4().to_string(),
        "content":content,"stream":"test","level":0,"score":1,"timestamp":1000,
        "consolidated":false,"dormant":false,"in_progress":false}))
        .unwrap();
    store.store_chunk(&chunk).unwrap();
    chunk
}
fn context<'a>(chunk: &'a Chunk, pii: &'a PiiFilter) -> GraphContributionContext<'a> {
    GraphContributionContext {
        stream: &chunk.stream,
        chunk_id: &chunk.id,
        expected_content: &chunk.content,
        pii,
    }
}

#[test]
fn removed_canonical_and_alias_do_not_resolve_surviving_shared_entity() {
    let (_temp, store, graph, pii) = setup(false);
    let mut first = chunk(&store, "first");
    let second = chunk(&store, "second");
    let node = graph
        .link_entity(context(&first, &pii).entity(
            "Former",
            "Person",
            &["Shared".into(), "Retired".into()],
        ))
        .unwrap();
    let shared = graph
        .link_entity(context(&second, &pii).entity(
            "Retained",
            "Person",
            &["Shared".into(), "Current".into()],
        ))
        .unwrap();
    assert_eq!(node.id, shared.id);
    first.deleted_at = Some(2000);
    store.store_chunk(&first).unwrap();
    let live = graph
        .get_live_entity_by_name("Current", "test")
        .unwrap()
        .unwrap();
    assert_eq!(live.id, node.id);
    assert_eq!(live.canonical_name, "Retained");
    for retired in ["Former", "Retired"] {
        assert!(graph.get_entity_by_name(retired, "test").unwrap().is_some());
        assert!(graph
            .get_live_entity_by_name(retired, "test")
            .unwrap()
            .is_none());
        assert!(!live.aliases.iter().any(|alias| alias == retired));
    }
}

#[test]
fn live_edge_requires_attributed_relation_and_both_live_endpoint_claims() {
    let (_temp, store, graph, pii) = setup(false);
    let mut first = chunk(&store, "old");
    let second = chunk(&store, "live");
    let left = graph
        .link_entity(context(&first, &pii).entity("Left", "Person", &[]))
        .unwrap();
    let right = graph
        .link_entity(context(&first, &pii).entity("Right", "Organization", &[]))
        .unwrap();
    graph
        .link_relation(context(&first, &pii).relation(&left.id, &right.id, "works_at"))
        .unwrap();
    graph
        .link_entity(context(&second, &pii).entity("Left", "Person", &[]))
        .unwrap();
    graph
        .link_entity(context(&second, &pii).entity("Right", "Organization", &[]))
        .unwrap();
    graph
        .link_relation(context(&second, &pii).relation(&left.id, &right.id, "uses"))
        .unwrap();
    first.deleted_at = Some(2000);
    store.store_chunk(&first).unwrap();
    let neighbors = graph.get_live_neighbors(&left.id).unwrap();
    assert_eq!(neighbors.len(), 1);
    assert_eq!(neighbors[0].0.relation_type, "uses");
}

#[test]
fn sidecars_are_encrypted_sanitized_and_corruption_never_falls_back() {
    let (_temp, store, graph, pii) = setup(true);
    let chunk = chunk(&store, "source");
    let node = graph
        .link_entity(context(&chunk, &pii).entity(
            "<b>Entity</b>",
            "Person",
            &["synthetic@example.test".into()],
        ))
        .unwrap();
    let key = label_key(&node.id, &chunk.id);
    let bytes = store.get(key.as_bytes()).unwrap().unwrap();
    assert!(crate::crypto::at_rest::is_encrypted(&bytes));
    assert!(!String::from_utf8_lossy(&bytes).contains("synthetic@example.test"));
    let live = graph.get_live_entity_by_id(&node.id).unwrap().unwrap();
    assert_eq!(live.canonical_name, "Entity");
    assert!(!live.aliases.join(" ").contains("synthetic@example.test"));
    store
        .store_entities(&chunk.id, "test", &[("Entity".into(), "Person".into())])
        .unwrap();
    store.put(key.as_bytes(), b"{}").unwrap();
    assert!(graph.get_live_entity_by_id(&node.id).is_err());
}

#[test]
fn obsolete_content_and_deleted_or_foreign_sources_cannot_publish() {
    let (_temp, store, graph, pii) = setup(false);
    let mut source = chunk(&store, "old");
    let node = graph
        .link_entity(context(&source, &pii).entity("Before", "Person", &[]))
        .unwrap();
    let old = source.clone();
    source.content = "new".into();
    store.store_chunk(&source).unwrap();
    assert!(graph.get_live_entity_by_id(&node.id).unwrap().is_none());
    assert!(graph
        .link_entity(context(&old, &pii).entity("Delayed", "Person", &[]))
        .is_err());
    let foreign = GraphContributionContext {
        stream: "foreign",
        chunk_id: &source.id,
        expected_content: &source.content,
        pii: &pii,
    };
    assert!(graph
        .link_entity(foreign.entity("Foreign", "Person", &[]))
        .is_err());
    source.deleted_at = Some(2000);
    store.store_chunk(&source).unwrap();
    assert!(graph
        .link_entity(context(&source, &pii).entity("Dead", "Person", &[]))
        .is_err());
}

#[test]
fn legacy_names_require_explicit_cache_proof_and_aliases_are_not_inferred() {
    let (_temp, store, graph, _pii) = setup(false);
    let source = chunk(&store, "May 2025 is mentioned, not a person alias proof");
    let node = graph
        .get_or_create_entity("Legacy", "Person", &["May".into()], "test")
        .unwrap();
    graph.add_chunk_to_entity(&node.id, &source.id).unwrap();
    assert!(graph.get_live_entity_by_id(&node.id).unwrap().is_none());
    store
        .store_entities(&source.id, "test", &[("Legacy".into(), "Person".into())])
        .unwrap();
    let live = graph
        .get_live_entity_by_name("Legacy", "test")
        .unwrap()
        .unwrap();
    assert_eq!(live.entity_type, "Unknown");
    assert!(graph
        .get_live_entity_by_name("May", "test")
        .unwrap()
        .is_none());
}

#[test]
fn incoming_alias_collision_cannot_reassign_another_entity_index() {
    let (_temp, store, graph, pii) = setup(false);
    let source = chunk(&store, "source");
    let left = graph
        .link_entity(context(&source, &pii).entity("Alpha", "Organization", &[]))
        .unwrap();
    let right = graph
        .link_entity(context(&source, &pii).entity("Beta", "Organization", &[]))
        .unwrap();
    assert!(graph
        .link_entity(context(&source, &pii).entity("Alpha", "Organization", &["Beta".into()]))
        .is_err());
    assert_eq!(
        graph
            .get_live_entity_by_name("Alpha", "test")
            .unwrap()
            .unwrap()
            .id,
        left.id
    );
    assert_eq!(
        graph
            .get_live_entity_by_name("Beta", "test")
            .unwrap()
            .unwrap()
            .id,
        right.id
    );
}

#[test]
fn content_aba_barrier_rejects_old_work_and_old_claims_do_not_revive() {
    let (_temp, store, graph, pii) = setup(false);
    let original = chunk(&store, "A");
    let target = graph
        .capture_contribution_target(&original.id, "test", "A")
        .unwrap();
    let entity = graph
        .link_entity_if_current(
            context(&original, &pii).entity("Old", "Person", &[]),
            &target,
        )
        .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    std::thread::scope(|scope| {
        let gate = barrier.clone();
        let graph_ref = &graph;
        let original_ref = &original;
        let target_ref = &target;
        let pii_ref = &pii;
        scope.spawn(move || {
            gate.wait();
            gate.wait();
            assert!(graph_ref
                .link_entity_if_current(
                    context(original_ref, pii_ref).entity("Delayed", "Person", &[]),
                    target_ref
                )
                .is_err());
            assert!(graph_ref
                .publish_extraction_metadata(
                    target_ref,
                    &[("Delayed".into(), "Person".into())],
                    &[],
                    pii_ref
                )
                .is_err());
        });
        barrier.wait();
        let mut changed = original.clone();
        changed.content = "B".into();
        store.store_chunk(&changed).unwrap();
        store.store_chunk(&original).unwrap();
        barrier.wait();
    });
    assert!(graph.get_live_entity_by_id(&entity.id).unwrap().is_none());
    let fresh = graph
        .capture_contribution_target(&original.id, "test", "A")
        .unwrap();
    graph
        .link_entity_if_current(
            context(&original, &pii).entity("Old", "Person", &["Fresh".into()]),
            &fresh,
        )
        .unwrap();
    assert!(graph
        .get_live_entity_by_name("Fresh", "test")
        .unwrap()
        .is_some());
    assert!(graph
        .get_live_entity_by_name("Delayed", "test")
        .unwrap()
        .is_none());
}

#[test]
fn delete_recreate_same_content_barrier_rejects_old_work_and_old_sidecars() {
    let (_temp, store, graph, pii) = setup(false);
    let source = chunk(&store, "same");
    let target = graph
        .capture_contribution_target(&source.id, "test", "same")
        .unwrap();
    let entity = graph
        .link_entity_if_current(context(&source, &pii).entity("Old", "Person", &[]), &target)
        .unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    std::thread::scope(|scope| {
        let gate = barrier.clone();
        let graph_ref = &graph;
        let source_ref = &source;
        let target_ref = &target;
        let pii_ref = &pii;
        scope.spawn(move || {
            gate.wait();
            gate.wait();
            assert!(graph_ref
                .link_entity_if_current(
                    context(source_ref, pii_ref).entity("Delayed", "Person", &[]),
                    target_ref
                )
                .is_err());
        });
        barrier.wait();
        store.hard_delete_by_id(&source.id).unwrap();
        store.store_chunk(&source).unwrap();
        barrier.wait();
    });
    assert!(graph.get_live_entity_by_id(&entity.id).unwrap().is_none());
    assert!(graph
        .publish_extraction_metadata(&target, &[], &[], &pii)
        .is_err());
}

#[test]
fn same_stream_ciphertext_cannot_be_copied_between_entities_or_edges() {
    let (_temp, store, graph, pii) = setup(true);
    let source = chunk(&store, "same");
    let a = graph
        .link_entity(context(&source, &pii).entity("Alpha", "Organization", &[]))
        .unwrap();
    let b = graph
        .link_entity(context(&source, &pii).entity("Beta", "Organization", &[]))
        .unwrap();
    let c = graph
        .link_entity(context(&source, &pii).entity("Gamma", "Organization", &[]))
        .unwrap();
    let ab = graph
        .link_relation(context(&source, &pii).relation(&a.id, &b.id, "uses"))
        .unwrap();
    let ac = graph
        .link_relation(context(&source, &pii).relation(&a.id, &c.id, "uses"))
        .unwrap();
    let original = store
        .get(relation_key(&ab.id, &source.id).as_bytes())
        .unwrap()
        .unwrap();
    store
        .put(relation_key(&ac.id, &source.id).as_bytes(), &original)
        .unwrap();
    assert!(graph.get_live_neighbors(&a.id).is_err());
    let original = store
        .get(label_key(&a.id, &source.id).as_bytes())
        .unwrap()
        .unwrap();
    store
        .put(label_key(&b.id, &source.id).as_bytes(), &original)
        .unwrap();
    assert!(graph.get_live_entity_by_id(&b.id).is_err());
}

#[test]
fn returned_corrupt_edge_cannot_receive_a_contribution() {
    let (_temp, store, graph, pii) = setup(false);
    let source = chunk(&store, "source");
    let a = graph
        .link_entity(context(&source, &pii).entity("Alpha", "Organization", &[]))
        .unwrap();
    let b = graph
        .link_entity(context(&source, &pii).entity("Beta", "Organization", &[]))
        .unwrap();
    let c = graph
        .link_entity(context(&source, &pii).entity("Gamma", "Organization", &[]))
        .unwrap();
    let edge = graph
        .get_or_create_edge(&a.id, &b.id, "uses", "test")
        .unwrap();
    let mut corrupt = edge.clone();
    corrupt.source_entity_id = c.id;
    store
        .put(
            format!("graph:edge:{}", edge.id).as_bytes(),
            &serde_json::to_vec(&corrupt).unwrap(),
        )
        .unwrap();
    assert!(graph
        .link_relation(context(&source, &pii).relation(&a.id, &b.id, "uses"))
        .is_err());
    corrupt.source_entity_id = a.id.clone();
    corrupt.stream_id = "foreign".into();
    store
        .put(
            format!("graph:edge:{}", edge.id).as_bytes(),
            &serde_json::to_vec(&corrupt).unwrap(),
        )
        .unwrap();
    assert!(graph
        .link_relation(context(&source, &pii).relation(&a.id, &b.id, "uses"))
        .is_err());
    assert!(store
        .get(relation_key(&edge.id, &source.id).as_bytes())
        .unwrap()
        .is_none());
}

#[test]
fn guarded_metadata_publication_is_sanitized_and_marker_has_no_model_content() {
    let (_temp, store, graph, pii) = setup(false);
    let source = chunk(&store, "source");
    let target = graph
        .capture_contribution_target(&source.id, "test", "source")
        .unwrap();
    graph
        .publish_extraction_metadata(
            &target,
            &[("<b>synthetic@example.test</b>".into(), "Person".into())],
            &[(
                "synthetic@example.test".into(),
                "uses".into(),
                "<b>Target</b>".into(),
            )],
            &pii,
        )
        .unwrap();
    let entities = store.get_entities(&source.id, "test").unwrap().join(" ");
    assert!(!entities.contains("synthetic@example.test"));
    assert!(!entities.contains("<b>"));
    assert_eq!(
        store
            .get(format!("llm_entities:{}", source.id).as_bytes())
            .unwrap()
            .unwrap(),
        b"1"
    );
}

#[test]
fn dictionary_metadata_does_not_mark_model_completion_and_index_rejects_obsolete_target() {
    let (temp, store, graph, pii) = setup(false);
    let mut source = chunk(&store, "source");
    let target = graph
        .capture_contribution_target(&source.id, "test", "source")
        .unwrap();
    graph
        .publish_dictionary_metadata(&target, &[("Proof".into(), "Person".into())], &[], &pii)
        .unwrap();
    assert!(store
        .get(format!("llm_entities:{}", source.id).as_bytes())
        .unwrap()
        .is_none());
    let config = crate::tantivy_index::TantivyConfig {
        enabled: true,
        heap_size_mb: 16,
        drift_warn_pct: 5.0,
        auto_rebuild_on_drift: false,
    };
    let mut index =
        crate::tantivy_index::TantivyIndex::open(&temp.path().join("index"), &config).unwrap();
    graph
        .reindex_extraction_if_current(&target, &mut index)
        .unwrap();
    index.commit().unwrap();
    assert_eq!(index.count().unwrap(), 1);
    source.content = "changed".into();
    store.store_chunk(&source).unwrap();
    assert!(graph
        .reindex_extraction_if_current(&target, &mut index)
        .is_err());
}

#[test]
fn legacy_explicit_event_date_survives_reindex_instead_of_becoming_recorded_time() {
    let (temp, store, graph, _pii) = setup(false);
    let mut source = chunk(&store, "legacy dated event");
    source.timestamp = 1_700_000_000;
    source.extraction_meta = Some(
        serde_json::from_value(serde_json::json!({
            "fact_type":"fact","confidence":1.0,"event_date":"2020-02-03"
        }))
        .unwrap(),
    );
    store.store_chunk(&source).unwrap();
    let target = graph
        .capture_contribution_target(&source.id, "test", &source.content)
        .unwrap();
    let config = crate::tantivy_index::TantivyConfig {
        enabled: true,
        heap_size_mb: 16,
        drift_warn_pct: 5.0,
        auto_rebuild_on_drift: false,
    };
    let mut index =
        crate::tantivy_index::TantivyIndex::open(&temp.path().join("legacy-index"), &config)
            .unwrap();
    graph
        .reindex_extraction_if_current(&target, &mut index)
        .unwrap();
    index.commit().unwrap();
    let event_noon = chrono::NaiveDate::from_ymd_opt(2020, 2, 3)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp();
    let hits = index
        .search_with_date_range("", event_noon, event_noon, Some("test"), 10)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, source.id);
    assert!(index
        .search_with_date_range("", 1_700_000_000, 1_700_000_000, Some("test"), 10)
        .unwrap()
        .is_empty());
    let midnight = event_noon - 12 * 60 * 60;
    assert!(index
        .search_with_date_range("", midnight, midnight, Some("test"), 10)
        .unwrap()
        .is_empty());
}

#[test]
fn rekey_preserves_live_incoming_canonical_name_before_and_after_original_source_deletion() {
    for encrypted in [false, true] {
        let (_temp, store, graph, pii) = setup(encrypted);
        let mut former = chunk(&store, "former source");
        let retained = chunk(&store, "retained source");
        let first = graph
            .link_entity(context(&former, &pii).entity(
                "Former",
                "Person",
                &["Shared".into(), "RetiredAlias".into()],
            ))
            .unwrap();
        let current = graph
            .link_entity(context(&retained, &pii).entity(
                "Retained",
                "Person",
                &["Shared".into(), "CurrentAlias".into()],
            ))
            .unwrap();
        assert_eq!(first.id, current.id);
        assert!(graph
            .get_entity_by_id(&first.id)
            .unwrap()
            .unwrap()
            .aliases
            .iter()
            .any(|label| label == "Retained"));
        assert_eq!(
            graph
                .get_live_entity_by_name("Retained", "test")
                .unwrap()
                .unwrap()
                .id,
            first.id
        );
        graph.rekey_name_index().unwrap();
        assert_eq!(
            graph
                .get_live_entity_by_name("Retained", "test")
                .unwrap()
                .unwrap()
                .id,
            first.id
        );
        former.deleted_at = Some(2000);
        store.store_chunk(&former).unwrap();
        graph.rekey_name_index().unwrap();
        graph.rekey_name_index().unwrap();
        let live = graph
            .get_live_entity_by_name("Retained", "test")
            .unwrap()
            .unwrap();
        assert_eq!(live.canonical_name, "Retained");
        assert_eq!(live.id, first.id);
        for deleted in ["Former", "RetiredAlias"] {
            assert!(graph
                .get_live_entity_by_name(deleted, "test")
                .unwrap()
                .is_none());
            assert!(!live.aliases.iter().any(|label| label == deleted));
        }
    }
}
