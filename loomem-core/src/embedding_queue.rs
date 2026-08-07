use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::LlmConfig;
use crate::llm;
use crate::local_embeddings::LocalEmbedder;
use crate::storage::RocksDbStore;

/// A request to embed a chunk's content and store it.
#[derive(Debug)]
struct EmbedRequest {
    chunk_id: String,
    content: String,
}

/// Handle for submitting embedding requests to the background queue.
#[derive(Clone)]
pub struct EmbeddingQueue {
    tx: mpsc::Sender<EmbedRequest>,
}

impl EmbeddingQueue {
    /// Submit a chunk for background embedding. Non-blocking, returns immediately.
    /// Returns Err only if the queue is full or closed.
    pub fn enqueue(&self, chunk_id: String, content: String) -> Result<()> {
        self.tx
            .try_send(EmbedRequest { chunk_id, content })
            .map_err(|e| anyhow::anyhow!("Embedding queue full or closed: {}", e))
    }
}

/// Configuration for the embedding queue worker.
pub struct EmbeddingQueueConfig {
    pub batch_size: usize,
    pub flush_interval_secs: u64,
    pub queue_capacity: usize,
}

impl Default for EmbeddingQueueConfig {
    fn default() -> Self {
        Self {
            batch_size: 50,
            flush_interval_secs: 5,
            queue_capacity: 1000,
        }
    }
}

/// Spawn the embedding queue background worker. Returns a handle for submitting requests.
pub fn spawn_worker(
    store: Arc<RocksDbStore>,
    http_client: reqwest::Client,
    llm_config: LlmConfig,
    config: EmbeddingQueueConfig,
    local_embedder: Option<Arc<LocalEmbedder>>,
) -> EmbeddingQueue {
    let (tx, rx) = mpsc::channel(config.queue_capacity);

    tokio::spawn(worker_loop(
        rx,
        store,
        http_client,
        llm_config,
        config,
        local_embedder,
    ));

    EmbeddingQueue { tx }
}

async fn worker_loop(
    mut rx: mpsc::Receiver<EmbedRequest>,
    store: Arc<RocksDbStore>,
    http_client: reqwest::Client,
    llm_config: LlmConfig,
    config: EmbeddingQueueConfig,
    local_embedder: Option<Arc<LocalEmbedder>>,
) {
    info!(
        "Embedding queue worker started (batch_size={}, flush_interval={}s)",
        config.batch_size, config.flush_interval_secs
    );

    let mut batch: Vec<EmbedRequest> = Vec::with_capacity(config.batch_size);
    let flush_interval = tokio::time::Duration::from_secs(config.flush_interval_secs);

    loop {
        // Collect items until batch is full or timeout
        let deadline = tokio::time::Instant::now() + flush_interval;

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() || batch.len() >= config.batch_size {
                break;
            }

            tokio::select! {
                item = rx.recv() => {
                    match item {
                        Some(req) => batch.push(req),
                        None => {
                            // Channel closed — flush remaining and exit
                            if !batch.is_empty() {
                                flush_batch(&mut batch, &store, &http_client, &llm_config, &local_embedder).await;
                            }
                            info!("Embedding queue worker shutting down");
                            return;
                        }
                    }
                }
                _ = tokio::time::sleep(remaining) => {
                    break;
                }
            }
        }

        if !batch.is_empty() {
            flush_batch(
                &mut batch,
                &store,
                &http_client,
                &llm_config,
                &local_embedder,
            )
            .await;
        }
    }
}

async fn flush_batch(
    batch: &mut Vec<EmbedRequest>,
    store: &RocksDbStore,
    http_client: &reqwest::Client,
    llm_config: &LlmConfig,
    local_embedder: &Option<Arc<LocalEmbedder>>,
) {
    let count = batch.len();
    debug!("Flushing embedding batch of {} items", count);

    let texts: Vec<String> = batch.iter().map(|r| r.content.clone()).collect();
    let results = embed_all(texts, http_client, llm_config, local_embedder).await;

    let mut stored = 0;
    let mut failed = 0;

    // Pairing is by index: `results` always holds exactly one entry per
    // request, so a failing chunk costs only itself (cycle /015). A chunk that
    // still fails is logged by id and left for the embed-missing backfill.
    for (req, result) in batch.drain(..).zip(results) {
        match result.and_then(|embedding| store.store_embedding(&req.chunk_id, embedding)) {
            Ok(()) => stored += 1,
            Err(e) => {
                warn!("Failed to embed/store chunk {}: {:#}", req.chunk_id, e);
                failed += 1;
            }
        }
    }

    if failed > 0 {
        warn!(
            "Embedding batch: {}/{} stored, {} failed",
            stored, count, failed
        );
    } else {
        debug!("Embedding batch: {} stored", stored);
    }
}

/// Embed every text, returning exactly `texts.len()` results in input order.
///
/// The local path is per-element (one bad chunk no longer discards the batch);
/// the OpenAI path has no per-element granularity, so a failed call is expanded
/// into one error per request rather than silently dropping them.
async fn embed_all(
    texts: Vec<String>,
    http_client: &reqwest::Client,
    llm_config: &LlmConfig,
    local_embedder: &Option<Arc<LocalEmbedder>>,
) -> Vec<Result<Vec<f32>>> {
    let count = texts.len();

    if let Some(embedder) = local_embedder {
        // ONNX inference is synchronous and CPU-bound; run it on the blocking
        // pool so it doesn't park the async worker thread under load.
        let embedder = Arc::clone(embedder);
        match tokio::task::spawn_blocking(move || embedder.embed_batch_lenient(&texts)).await {
            Ok(results) => pad_to(results, count, "local embedder returned no result"),
            Err(e) => all_failed(count, &format!("embedding batch task failed to join: {e}")),
        }
    } else {
        match llm::embed_batch(http_client, llm_config, &texts).await {
            Ok(vectors) => pad_to(
                vectors.into_iter().map(Ok).collect(),
                count,
                "embedding API returned fewer vectors than requests",
            ),
            Err(e) => all_failed(count, &format!("batch embedding API failed: {e:#}")),
        }
    }
}

/// Grow `results` to `count` entries so index pairing stays sound even if an
/// embedder returns a short vector.
fn pad_to(mut results: Vec<Result<Vec<f32>>>, count: usize, reason: &str) -> Vec<Result<Vec<f32>>> {
    while results.len() < count {
        results.push(Err(anyhow::anyhow!("{reason}")));
    }
    results
}

fn all_failed(count: usize, reason: &str) -> Vec<Result<Vec<f32>>> {
    pad_to(Vec::new(), count, reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pad_to_fills_missing_slots() {
        let padded = pad_to(vec![Ok(vec![0.5_f32])], 3, "short");
        assert_eq!(padded.len(), 3, "one entry per request");
        assert!(padded[0].is_ok());
        assert!(padded[1].is_err() && padded[2].is_err());
    }

    #[test]
    fn pad_to_keeps_longer_input_untouched() {
        let padded = pad_to(vec![Ok(vec![0.5_f32]), Ok(vec![0.25_f32])], 1, "short");
        assert_eq!(padded.len(), 2);
    }

    #[test]
    fn all_failed_yields_one_error_per_request() {
        let results = all_failed(4, "provider down");
        assert_eq!(results.len(), 4);
        assert!(results.iter().all(Result::is_err));
    }

    /// Cycle /015 end-to-end regression for the "buried entity" incident: a
    /// flush containing one oversized chunk used to store **zero** embeddings
    /// for the whole batch. Requires the real ONNX model.
    #[cfg(feature = "local-embeddings")]
    #[tokio::test]
    #[ignore = "requires LOOMEM_TEST_EMBED_MODEL"]
    async fn flush_stores_every_chunk_despite_oversized_member() {
        use crate::config::RocksDbConfig;

        let Ok(model_dir) = std::env::var("LOOMEM_TEST_EMBED_MODEL") else {
            eprintln!("skip: set LOOMEM_TEST_EMBED_MODEL=<dir> to run this test");
            return;
        };
        let embedder = LocalEmbedder::load(std::path::Path::new(&model_dir), 384)
            .expect("load local embedding model");

        let temp = tempfile::tempdir().expect("tempdir");
        let store = RocksDbStore::open(
            temp.path(),
            &RocksDbConfig {
                max_open_files: 100,
                compression: "lz4".to_string(),
                write_buffer_size: 4 * 1024 * 1024,
                max_write_buffer_number: 2,
            },
        )
        .expect("open store");

        let oversized = "Szczegółowy raport z zebrania zarządu w sprawie budżetu. ".repeat(90);
        let mut batch: Vec<EmbedRequest> = ["short-1", "short-2", "long-3", "short-4", "short-5"]
            .iter()
            .map(|id| EmbedRequest {
                chunk_id: (*id).to_string(),
                content: if *id == "long-3" {
                    oversized.clone()
                } else {
                    format!("Krótki fakt o projekcie numer {id}.")
                },
            })
            .collect();

        flush_batch(
            &mut batch,
            &store,
            &reqwest::Client::new(),
            &LlmConfig::default(),
            &Some(Arc::new(embedder)),
        )
        .await;

        assert!(batch.is_empty(), "flush must drain the batch");
        for id in ["short-1", "short-2", "long-3", "short-4", "short-5"] {
            let embedding = store.get_embedding(id).expect("read embedding");
            assert_eq!(
                embedding.map(|v| v.len()),
                Some(384),
                "chunk {id} must have an embedding"
            );
        }
    }
}
