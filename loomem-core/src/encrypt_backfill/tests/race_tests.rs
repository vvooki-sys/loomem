use super::*;
use crate::{
    graph::GraphContributionContext,
    pii_filter::{PiiConfig, PiiFilter},
};

#[tokio::test]
async fn stale_plaintext_scan_cannot_overwrite_new_encrypted_graph_claim() {
    let tmp = TempDir::new().unwrap();
    let stream = "race:stream";
    let chunk = make_chunk("race:chunk", stream);
    let (key, old_plaintext, entity_id) = {
        let store = Arc::new(open_noop(&tmp));
        store.store_chunk(&chunk).unwrap();
        let graph = GraphStore::new(Arc::clone(&store));
        let pii = PiiFilter::new(PiiConfig::default()).unwrap();
        let ctx = GraphContributionContext {
            stream,
            chunk_id: &chunk.id,
            expected_content: &chunk.content,
            pii: &pii,
        };
        let entity = graph
            .link_entity(ctx.entity("race_entity", "synthetic", &[]))
            .unwrap();
        let (key, value) = store.prefix_scan(b"graph:labels:").next().unwrap();
        assert!(!is_encrypted(&value));
        (key, value, entity.id)
    };
    let store = open_encrypted(&tmp);
    let graph = GraphStore::new(Arc::clone(&store));
    // Another migration/publisher wins after our scan but before our flush.
    run_encrypt_backfill(&store, &graph, &params(2), &trace(&tmp))
        .await
        .unwrap();
    graph.rekey_name_index().unwrap();
    let pii = PiiFilter::new(PiiConfig::default()).unwrap();
    let ctx = GraphContributionContext {
        stream,
        chunk_id: &chunk.id,
        expected_content: &chunk.content,
        pii: &pii,
    };
    graph
        .link_entity(ctx.entity("race_entity", "synthetic", &["new_claim_sentinel".into()]))
        .unwrap();
    let latest = store.get(&key).unwrap().unwrap();
    assert!(is_encrypted(&latest));
    let mut counters = ClassCounters::default();
    flush_raw_batch(
        &store,
        vec![(key.clone(), old_plaintext)],
        "graph_labels",
        ScopeSource::GraphPairedChunk,
        &mut counters,
    )
    .await
    .unwrap();
    assert_eq!(store.get(&key).unwrap().unwrap(), latest);
    assert_eq!(counters.scanned, 1);
    assert_eq!(counters.already_encrypted, 1);
    assert_eq!(counters.encrypted, 0);
    assert_eq!(counters.orphans, 0);
    assert_eq!(
        graph
            .get_live_entity_by_name("new_claim_sentinel", stream)
            .unwrap()
            .unwrap()
            .id,
        entity_id
    );
}

#[tokio::test]
async fn stale_scan_does_not_restore_deleted_sidecar_and_encrypts_current_plaintext() {
    let tmp = TempDir::new().unwrap();
    let store = open_encrypted(&tmp);
    let chunk = make_chunk("legacy:race:chunk", "current:stream");
    store.store_chunk(&chunk).unwrap();
    for prefix in ["graph:labels:", "graph:edge_labels:"] {
        let key = format!("{prefix}{}:{}", uuid::Uuid::new_v4(), chunk.id);
        let old = b"old snapshot plaintext".to_vec();
        store.put(key.as_bytes(), &old).unwrap();
        let snapshot = (
            key.as_bytes().to_vec().into_boxed_slice(),
            old.into_boxed_slice(),
        );
        store.put(key.as_bytes(), b"new current plaintext").unwrap();
        let mut counters = ClassCounters::default();
        flush_raw_batch(
            &store,
            vec![snapshot.clone()],
            "graph_sidecar",
            ScopeSource::GraphPairedChunk,
            &mut counters,
        )
        .await
        .unwrap();
        let encrypted = store.get(key.as_bytes()).unwrap().unwrap();
        assert!(is_encrypted(&encrypted));
        assert_eq!(
            store
                .encryption_provider()
                .decrypt(&chunk.stream, &encrypted)
                .unwrap(),
            b"new current plaintext"
        );
        assert_eq!(counters.encrypted, 1);
        store.delete(key.as_bytes()).unwrap();
        let mut deleted = ClassCounters::default();
        flush_raw_batch(
            &store,
            vec![snapshot],
            "graph_sidecar",
            ScopeSource::GraphPairedChunk,
            &mut deleted,
        )
        .await
        .unwrap();
        assert!(store.get(key.as_bytes()).unwrap().is_none());
        assert_eq!(deleted.scanned, 1);
        assert_eq!(deleted.encrypted, 0);
        assert_eq!(deleted.already_encrypted, 0);
        assert_eq!(deleted.orphans, 0);
    }
}

#[tokio::test]
async fn paired_cache_stale_scan_cannot_recreate_invalidated_proof_or_lose_current_bytes() {
    let tmp = TempDir::new().unwrap();
    let store = open_encrypted(&tmp);
    let mut chunk = make_chunk("legacy:proof:race", "test");
    store.store_chunk(&chunk).unwrap();
    for prefix in ["entity:", "rel:"] {
        let key = format!("{prefix}{}", chunk.id);
        let old = b"old proof snapshot".to_vec();
        store.put(key.as_bytes(), &old).unwrap();
        let snapshot = (
            key.as_bytes().to_vec().into_boxed_slice(),
            old.into_boxed_slice(),
        );
        chunk.content.push_str(" edited");
        store.store_chunk(&chunk).unwrap();
        let mut counters = ClassCounters::default();
        flush_raw_batch(
            &store,
            vec![snapshot.clone()],
            "paired",
            ScopeSource::PairedChunk,
            &mut counters,
        )
        .await
        .unwrap();
        assert!(store.get(key.as_bytes()).unwrap().is_none());
        assert_eq!(counters.encrypted, 0);
        store
            .put(key.as_bytes(), b"current replacement proof")
            .unwrap();
        flush_raw_batch(
            &store,
            vec![snapshot],
            "paired",
            ScopeSource::PairedChunk,
            &mut counters,
        )
        .await
        .unwrap();
        let value = store.get(key.as_bytes()).unwrap().unwrap();
        assert_eq!(
            store
                .encryption_provider()
                .decrypt(&chunk.stream, &value)
                .unwrap(),
            b"current replacement proof"
        );
        assert_eq!(counters.encrypted, 1);
    }
}

#[tokio::test]
async fn primary_chunk_snapshot_preserves_current_edit_tombstone_and_missing_row() {
    let tmp = TempDir::new().unwrap();
    let store = open_encrypted(&tmp);
    let mut chunk = make_chunk("primary:race", "test");
    let key = format!("chunk:L0:{}", chunk.id);
    let snapshot = (
        key.as_bytes().to_vec().into_boxed_slice(),
        serde_json::to_vec(&chunk).unwrap().into_boxed_slice(),
    );
    chunk.content = "new current content".into();
    chunk.deleted_at = Some(9);
    // Seed the precise legacy plaintext envelope seen by the migration.
    store
        .put(key.as_bytes(), &serde_json::to_vec(&chunk).unwrap())
        .unwrap();
    let mut counters = ClassCounters::default();
    flush_chunk_batch(&store, vec![snapshot.clone()], "chunk", &mut counters)
        .await
        .unwrap();
    let current = store.get_chunk(&chunk.id).unwrap().unwrap();
    assert_eq!(current.content, chunk.content);
    assert_eq!(current.deleted_at, Some(9));
    store.hard_delete_by_id(&chunk.id).unwrap();
    flush_chunk_batch(&store, vec![snapshot], "chunk", &mut counters)
        .await
        .unwrap();
    assert!(store.get_chunk(&chunk.id).unwrap().is_none());
}

#[tokio::test]
async fn primary_entity_snapshot_preserves_current_labels_links_and_absence() {
    let tmp = TempDir::new().unwrap();
    let store = open_encrypted(&tmp);
    let graph = GraphStore::new(Arc::clone(&store));
    let id = uuid::Uuid::new_v4();
    let key = format!("graph:entity:{id}");
    let old = serde_json::json!({"id":id,"canonical_name":"old","entity_type":"synthetic","aliases":[],"stream_id":"test","chunk_ids":["old"],"created_at":1,"updated_at":1});
    let snapshot = (
        key.as_bytes().to_vec().into_boxed_slice(),
        serde_json::to_vec(&old).unwrap().into_boxed_slice(),
    );
    let mut current = old;
    current["aliases"] = serde_json::json!(["new alias"]);
    current["chunk_ids"] = serde_json::json!(["old", "new"]);
    store
        .put(key.as_bytes(), &serde_json::to_vec(&current).unwrap())
        .unwrap();
    let mut counters = ClassCounters::default();
    flush_entity_batch(
        &store,
        &graph,
        vec![snapshot.clone()],
        "entity",
        &mut counters,
    )
    .await
    .unwrap();
    let entity = graph.get_entity_by_id(&id.to_string()).unwrap().unwrap();
    assert_eq!(entity.aliases, vec!["new alias"]);
    assert_eq!(entity.chunk_ids.len(), 2);
    store.delete(key.as_bytes()).unwrap();
    flush_entity_batch(&store, &graph, vec![snapshot], "entity", &mut counters)
        .await
        .unwrap();
    assert!(store.get(key.as_bytes()).unwrap().is_none());
    current["id"] = serde_json::json!(uuid::Uuid::new_v4());
    let mismatched = serde_json::to_vec(&current).unwrap();
    store.put(key.as_bytes(), &mismatched).unwrap();
    let snapshot = (
        key.as_bytes().to_vec().into_boxed_slice(),
        mismatched.clone().into_boxed_slice(),
    );
    flush_entity_batch(&store, &graph, vec![snapshot], "entity", &mut counters)
        .await
        .unwrap();
    assert_eq!(store.get(key.as_bytes()).unwrap().unwrap(), mismatched);
    assert_eq!(counters.orphans, 1);
}
