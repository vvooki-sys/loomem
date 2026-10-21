use super::*;
use crate::config::RocksDbConfig;
use crate::storage::Chunk;

fn fixture() -> (tempfile::TempDir, Worker, Chunk) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = RocksDbConfig {
        max_open_files: 100,
        compression: "none".into(),
        write_buffer_size: 4 * 1024 * 1024,
        max_write_buffer_number: 2,
    };
    let store = Arc::new(RocksDbStore::open(dir.path().join("data"), &cfg).unwrap());
    let chunk: Chunk = serde_json::from_value(serde_json::json!({
        "id":"queued-source", "content":"synthetic original", "stream":"test", "level":0,
        "score":1.0,"timestamp":1000,"consolidated":false,"dormant":false,"in_progress":false
    }))
    .unwrap();
    store.store_chunk(&chunk).unwrap();
    let http_client = reqwest::Client::new();
    let context = EntityExtractionWorkerContext {
        llm_config: LlmConfig::default(),
        cost_tracker: Arc::new(CostTracker::new(
            store.clone(),
            Default::default(),
            http_client.clone(),
        )),
        config: EntityExtractionConfig::default(),
        pii_filter: Arc::new(PiiFilter::new(Default::default()).unwrap()),
    };
    let index = TantivyIndex::open(
        dir.path().join("index"),
        &crate::config::TantivyConfig {
            enabled: true,
            heap_size_mb: 16,
            drift_warn_pct: 5.0,
            auto_rebuild_on_drift: false,
        },
    )
    .unwrap();
    let worker = Worker {
        graph: Arc::new(GraphStore::new(store.clone())),
        store,
        tantivy: Arc::new(tokio::sync::Mutex::new(index)),
        http_client,
        context,
    };
    (dir, worker, chunk)
}

async fn queued(worker: &Worker, chunk: &Chunk) -> ExtractRequest {
    let (tx, mut rx) = mpsc::channel(1);
    let queue = EntityExtractionQueue {
        tx,
        graph: worker.graph.clone(),
    };
    queue
        .enqueue(
            chunk.id.clone(),
            chunk.content.clone(),
            chunk.stream.clone(),
            Vec::new(),
        )
        .unwrap();
    rx.recv().await.unwrap()
}
fn result(id: &str) -> llm_ner::ChunkExtraction {
    serde_json::from_value(serde_json::json!({"chunk_id":id,"entities":[{
        "name":"Cedar <b>Laboratory</b>","entity_type":"Organization", "confidence":1.0,
        "aliases":["synthetic@example.test"]}],"relations":[]}))
    .unwrap()
}

#[tokio::test]
async fn queued_publication_sanitizes_cache_graph_and_static_marker() {
    let (_dir, worker, chunk) = fixture();
    let request = queued(&worker, &chunk).await;
    publication::publish(&worker, &request, &result(&chunk.id))
        .await
        .unwrap();
    let entity = worker
        .graph
        .get_live_entity_by_name("Cedar Laboratory", "test")
        .unwrap()
        .unwrap();
    assert!(entity
        .aliases
        .iter()
        .all(|value| !value.contains("synthetic@example.test")));
    assert_eq!(
        worker
            .store
            .get(format!("llm_entities:{}", chunk.id).as_bytes())
            .unwrap()
            .unwrap(),
        b"1"
    );
    assert!(worker
        .store
        .get_entities(&chunk.id, &chunk.stream)
        .unwrap()
        .iter()
        .all(|value| !value.contains('<')));
}

async fn reject_delayed(recreate: bool) {
    let (_dir, worker, mut chunk) = fixture();
    let request = queued(&worker, &chunk).await;
    let store = worker.store.clone();
    let graph = worker.graph.clone();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    // A queued request already captured its generation before provider execution.
    let job = tokio::spawn(async move {
        entered_tx.send(()).unwrap();
        release_rx.await.unwrap();
        publication::publish(&worker, &request, &result(&request.chunk_id)).await
    });
    entered_rx.await.unwrap();
    let original = chunk.content.clone();
    if recreate {
        store.delete_by_id(&chunk.id).unwrap();
    } else {
        chunk.content = "synthetic replacement".into();
        store.store_chunk(&chunk).unwrap();
    }
    chunk.content = original;
    store.store_chunk(&chunk).unwrap();
    release_tx.send(()).unwrap();
    assert!(job.await.unwrap().is_err());
    assert!(store
        .get_entities(&chunk.id, &chunk.stream)
        .unwrap()
        .is_empty());
    assert!(store
        .get(format!("llm_entities:{}", chunk.id).as_bytes())
        .unwrap()
        .is_none());
    assert_eq!(graph.count_entities().unwrap(), 0);
}
#[tokio::test]
async fn queued_old_provider_result_cannot_publish_after_content_aba() {
    reject_delayed(false).await;
}
#[tokio::test]
async fn queued_old_provider_result_cannot_publish_after_delete_recreate() {
    reject_delayed(true).await;
}
