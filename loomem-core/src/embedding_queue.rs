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

    // Use local embedder if available, otherwise OpenAI API
    let embed_result = if let Some(ref embedder) = local_embedder {
        // ONNX inference is synchronous and CPU-bound; run it on the blocking
        // pool so it doesn't park the async worker thread under load.
        let embedder = Arc::clone(embedder);
        match tokio::task::spawn_blocking(move || embedder.embed_batch(&texts)).await {
            Ok(res) => res,
            Err(e) => Err(anyhow::anyhow!("embedding batch task failed to join: {e}")),
        }
    } else {
        llm::embed_batch(http_client, llm_config, &texts).await
    };

    match embed_result {
        Ok(embeddings) => {
            let mut stored = 0;
            let mut failed = 0;

            for (req, embedding) in batch.drain(..).zip(embeddings) {
                match store.store_embedding(&req.chunk_id, embedding) {
                    Ok(()) => stored += 1,
                    Err(e) => {
                        warn!("Failed to store embedding for {}: {}", req.chunk_id, e);
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
        Err(e) => {
            warn!("Batch embedding API failed for {} items: {}", count, e);
            // Clear batch — items are lost but embed-missing handler can backfill
            batch.clear();
        }
    }
}
