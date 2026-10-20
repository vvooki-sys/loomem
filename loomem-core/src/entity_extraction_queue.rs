use crate::config::{EntityExtractionConfig, LlmConfig};
use crate::cost_tracker::CostTracker;
use crate::graph::{GraphContributionTarget, GraphStore};
use crate::{llm_ner, pii_filter::PiiFilter, storage::RocksDbStore, tantivy_index::TantivyIndex};
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
mod publication;
#[cfg(test)]
mod tests;

struct ExtractRequest {
    chunk_id: String,
    content: String,
    stream: String,
    dict_entities: Vec<(String, String)>,
    target: GraphContributionTarget,
}

#[derive(Clone)]
pub struct EntityExtractionQueue {
    tx: mpsc::Sender<ExtractRequest>,
    graph: Arc<GraphStore>,
}
impl EntityExtractionQueue {
    pub fn enqueue(
        &self,
        chunk_id: String,
        content: String,
        stream: String,
        dict_entities: Vec<(String, String)>,
    ) -> Result<()> {
        let target = self
            .graph
            .capture_contribution_target(&chunk_id, &stream, &content)?;
        self.tx
            .try_send(ExtractRequest {
                chunk_id,
                content,
                stream,
                dict_entities,
                target,
            })
            .map_err(|e| anyhow::anyhow!("Entity extraction queue full or closed: {}", e))
    }
}

/// Configured privacy policy follows asynchronous extraction through publication.
pub struct EntityExtractionWorkerContext {
    pub llm_config: LlmConfig,
    pub cost_tracker: Arc<CostTracker>,
    pub config: EntityExtractionConfig,
    pub pii_filter: Arc<PiiFilter>,
}
struct Worker {
    store: Arc<RocksDbStore>,
    graph: Arc<GraphStore>,
    tantivy: Arc<tokio::sync::Mutex<TantivyIndex>>,
    http_client: reqwest::Client,
    context: EntityExtractionWorkerContext,
}
pub fn spawn_worker(
    store: Arc<RocksDbStore>,
    graph: Arc<GraphStore>,
    tantivy: Arc<tokio::sync::Mutex<TantivyIndex>>,
    http_client: reqwest::Client,
    context: EntityExtractionWorkerContext,
) -> EntityExtractionQueue {
    let (tx, rx) = mpsc::channel(context.config.queue_capacity);
    let queue = EntityExtractionQueue {
        tx,
        graph: graph.clone(),
    };
    tokio::spawn(worker_loop(
        rx,
        Worker {
            store,
            graph,
            tantivy,
            http_client,
            context,
        },
    ));
    queue
}
async fn worker_loop(mut rx: mpsc::Receiver<ExtractRequest>, worker: Worker) {
    let config = &worker.context.config;
    info!(
        "Entity extraction queue started (batch_size={}, flush={}s, confidence={})",
        config.batch_size, config.flush_interval_secs, config.confidence_threshold
    );

    let mut batch: Vec<ExtractRequest> = Vec::with_capacity(config.batch_size);
    let mut batch_tokens: usize = 0;
    let flush_interval = tokio::time::Duration::from_secs(config.flush_interval_secs);

    loop {
        let deadline = tokio::time::Instant::now() + flush_interval;

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero()
                || batch.len() >= config.batch_size
                || batch_tokens >= config.max_tokens_per_batch
            {
                break;
            }

            tokio::select! {
                item = rx.recv() => {
                    match item {
                        Some(req) => {
                            batch_tokens += llm_ner::estimate_tokens(&req.content);
                            batch.push(req);
                        }
                        None => {
                            if !batch.is_empty() {
                                flush_batch(&mut batch, &mut batch_tokens, &worker).await;
                            }
                            info!("Entity extraction queue shutting down");
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
            flush_batch(&mut batch, &mut batch_tokens, &worker).await;
        }
    }
}

async fn flush_batch(batch: &mut Vec<ExtractRequest>, tokens: &mut usize, worker: &Worker) {
    if let Err(error) = extract_batch(batch, worker).await {
        warn!(%error, "Entity extraction batch failed");
    }
    batch.clear();
    *tokens = 0;
}

async fn extract_batch(batch: &[ExtractRequest], worker: &Worker) -> Result<()> {
    worker.context.cost_tracker.check_budget()?;
    let Some(key) = worker.context.llm_config.get_api_key() else {
        return Ok(());
    };
    let chunks: Vec<_> = batch
        .iter()
        .filter(|request| request_is_live(worker, request))
        .map(|request| {
            llm_ner::prepare_entry(
                &request.chunk_id,
                &request.content,
                &request.dict_entities,
                &worker.context.pii_filter,
            )
        })
        .collect();
    if chunks.is_empty() {
        return Ok(());
    }
    let config = &worker.context.config;
    let (extractions, input, output) = match llm_ner::extract_entities_llm(
        &worker.http_client,
        &config.model,
        &key,
        &chunks,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            crate::llm_failures::global().record(crate::llm_failures::LlmFailureKind::Ner);
            return Err(error);
        }
    };
    if let Err(error) = worker
        .context
        .cost_tracker
        .record(input, output, &config.model)
    {
        warn!(%error, "Failed to record entity extraction cost");
    }
    let (filtered, rejected) =
        llm_ner::filter_by_confidence(&extractions, config.confidence_threshold);
    debug!(
        count = filtered.len(),
        rejected = rejected.len(),
        "Entity extraction completed"
    );
    for extraction in &filtered {
        let Some(request) = batch
            .iter()
            .find(|request| request.chunk_id == extraction.chunk_id)
        else {
            continue;
        };
        if let Err(error) = publication::publish(worker, request, extraction).await {
            warn!(%error, "Entity extraction publication rejected");
        }
    }
    worker.tantivy.lock().await.commit()?;
    Ok(())
}

fn request_is_live(worker: &Worker, request: &ExtractRequest) -> bool {
    worker
        .store
        .get_chunk(&request.chunk_id)
        .ok()
        .flatten()
        .is_some_and(|chunk| {
            chunk.deleted_at.is_none()
                && chunk.stream == request.stream
                && chunk.content == request.content
        })
}
