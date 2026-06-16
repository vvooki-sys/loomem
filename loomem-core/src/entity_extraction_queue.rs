use anyhow::Result;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::{EntityExtractionConfig, LlmConfig};
use crate::cost_tracker::CostTracker;
use crate::graph::GraphStore;
use crate::llm_ner;
use crate::storage::RocksDbStore;
use crate::tantivy_index::{TantivyIndex, TextDocument};

#[derive(Debug)]
struct ExtractRequest {
    chunk_id: String,
    content: String,
    stream: String,
    dict_entities: Vec<(String, String)>,
}

#[derive(Clone)]
pub struct EntityExtractionQueue {
    tx: mpsc::Sender<ExtractRequest>,
}

impl EntityExtractionQueue {
    pub fn enqueue(
        &self,
        chunk_id: String,
        content: String,
        stream: String,
        dict_entities: Vec<(String, String)>,
    ) -> Result<()> {
        self.tx
            .try_send(ExtractRequest {
                chunk_id,
                content,
                stream,
                dict_entities,
            })
            .map_err(|e| anyhow::anyhow!("Entity extraction queue full or closed: {}", e))
    }
}

pub fn spawn_worker(
    store: Arc<RocksDbStore>,
    graph: Arc<GraphStore>,
    tantivy: Arc<tokio::sync::Mutex<TantivyIndex>>,
    http_client: reqwest::Client,
    llm_config: LlmConfig,
    cost_tracker: Arc<CostTracker>,
    config: EntityExtractionConfig,
) -> EntityExtractionQueue {
    let (tx, rx) = mpsc::channel(config.queue_capacity);

    tokio::spawn(worker_loop(
        rx,
        store,
        graph,
        tantivy,
        http_client,
        llm_config,
        cost_tracker,
        config,
    ));

    EntityExtractionQueue { tx }
}

async fn worker_loop(
    mut rx: mpsc::Receiver<ExtractRequest>,
    store: Arc<RocksDbStore>,
    graph: Arc<GraphStore>,
    tantivy: Arc<tokio::sync::Mutex<TantivyIndex>>,
    http_client: reqwest::Client,
    llm_config: LlmConfig,
    cost_tracker: Arc<CostTracker>,
    config: EntityExtractionConfig,
) {
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
                                flush_batch(&mut batch, &mut batch_tokens, &store, &graph, &tantivy,
                                    &http_client, &llm_config, &cost_tracker, &config).await;
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
            flush_batch(
                &mut batch,
                &mut batch_tokens,
                &store,
                &graph,
                &tantivy,
                &http_client,
                &llm_config,
                &cost_tracker,
                &config,
            )
            .await;
        }
    }
}

async fn flush_batch(
    batch: &mut Vec<ExtractRequest>,
    batch_tokens: &mut usize,
    store: &RocksDbStore,
    graph: &GraphStore,
    tantivy: &Arc<tokio::sync::Mutex<TantivyIndex>>,
    http_client: &reqwest::Client,
    llm_config: &LlmConfig,
    cost_tracker: &CostTracker,
    config: &EntityExtractionConfig,
) {
    let count = batch.len();
    debug!(
        "Flushing entity extraction batch of {} items (~{} tokens)",
        count, batch_tokens
    );

    // Budget check
    if let Err(e) = cost_tracker.check_budget() {
        warn!("Entity extraction skipped (budget exceeded): {}", e);
        batch.clear();
        *batch_tokens = 0;
        return;
    }

    let api_key = match llm_config.get_api_key() {
        Some(k) => k,
        None => {
            warn!("Entity extraction skipped: no API key");
            batch.clear();
            *batch_tokens = 0;
            return;
        }
    };

    // Build batch for LLM
    let chunks: Vec<llm_ner::ChunkBatchEntry> = batch
        .iter()
        .map(|r| {
            (
                r.chunk_id.clone(),
                r.content.clone(),
                r.dict_entities.clone(),
            )
        })
        .collect();

    // Call LLM
    match llm_ner::extract_entities_llm(http_client, &config.model, &api_key, &chunks).await {
        Ok((extractions, input_tokens, output_tokens)) => {
            // Record cost
            if let Err(e) = cost_tracker.record(input_tokens, output_tokens, &config.model) {
                warn!("Failed to record entity extraction cost: {}", e);
            }

            // Filter by confidence
            let (filtered, rejected) =
                llm_ner::filter_by_confidence(&extractions, config.confidence_threshold);

            // Log rejected for calibration
            if !rejected.is_empty() {
                debug!(
                    "Entity extraction: {} entities rejected (below {:.1} confidence)",
                    rejected.len(),
                    config.confidence_threshold
                );
                for (cid, ent) in &rejected {
                    debug!(
                        "  Rejected: chunk={} entity='{}' confidence={:.2}",
                        cid, ent.name, ent.confidence
                    );
                }
            }

            // Merge and store
            let mut new_entities = 0;
            let mut new_relations = 0;

            for extraction in &filtered {
                let req = match batch.iter().find(|r| r.chunk_id == extraction.chunk_id) {
                    Some(r) => r,
                    None => continue,
                };

                // Merge entities
                let dict_names: HashSet<String> = req
                    .dict_entities
                    .iter()
                    .map(|(n, _)| n.to_lowercase())
                    .collect();

                for ent in &extraction.entities {
                    if dict_names.contains(&ent.name.to_lowercase()) {
                        continue;
                    }
                    // Check graph for existing entity (alias dedup, stream-scoped)
                    if let Ok(Some(existing)) = graph.get_entity_by_name(&ent.name, &req.stream) {
                        let _ = graph.add_chunk_to_entity(&existing.id, &extraction.chunk_id);
                        continue;
                    }

                    // New entity — create in graph (stream-scoped)
                    match graph.get_or_create_entity(
                        &ent.name,
                        &ent.entity_type,
                        &ent.aliases,
                        &req.stream,
                    ) {
                        Ok(node) => {
                            let _ = graph.add_chunk_to_entity(&node.id, &extraction.chunk_id);
                            new_entities += 1;
                        }
                        Err(e) => warn!("Failed to create graph entity '{}': {}", ent.name, e),
                    }
                }

                // Merge relations (stream-scoped)
                for rel in &extraction.relations {
                    if let (Ok(Some(src)), Ok(Some(tgt))) = (
                        graph.get_entity_by_name(&rel.subject, &req.stream),
                        graph.get_entity_by_name(&rel.object, &req.stream),
                    ) {
                        match graph.get_or_create_edge(&src.id, &tgt.id, &rel.relation, &req.stream)
                        {
                            Ok(edge) => {
                                let _ = graph.add_chunk_to_edge(&edge.id, &extraction.chunk_id);
                                new_relations += 1;
                            }
                            Err(e) => warn!("Failed to create graph edge: {}", e),
                        }
                    }
                }

                // Update RocksDB entity list (merge dict + LLM)
                let mut merged = req.dict_entities.clone();
                for ent in &extraction.entities {
                    if !dict_names.contains(&ent.name.to_lowercase()) {
                        merged.push((ent.name.clone(), ent.entity_type.clone()));
                    }
                }
                let _ = store.store_entities(&extraction.chunk_id, &req.stream, &merged);

                // Re-index in Tantivy with updated entities
                if let Ok(Some(chunk)) = store.get_chunk(&extraction.chunk_id) {
                    let entity_str = merged
                        .iter()
                        .map(|(n, _)| n.as_str())
                        .collect::<Vec<_>>()
                        .join(",");
                    let event_date_ts: Option<i64> = chunk
                        .extraction_meta
                        .as_ref()
                        .and_then(|m| m.event_date.as_ref())
                        .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
                        .map(|d| {
                            d.and_hms_opt(12, 0, 0)
                                .expect("valid static HMS")
                                .and_utc()
                                .timestamp()
                        });
                    let doc = TextDocument {
                        id: chunk.id.clone(),
                        content: chunk.content.clone(),
                        user_id: String::new(),
                        app_id: String::new(),
                        level: chunk.level,
                        timestamp: chunk.timestamp as i64,
                        stream: chunk.stream.clone(),
                        entities: Some(entity_str),
                        relations: None, // preserve existing
                        event_date: event_date_ts,
                        source_agent: chunk.source.as_ref().map(|s| s.agent.clone()),
                    };
                    let mut idx = tantivy.lock().await;
                    let _ = idx.upsert_document(doc);
                }

                // Store LLM extraction marker
                let marker_key = format!("llm_entities:{}", extraction.chunk_id);
                let marker_value = serde_json::to_vec(extraction).unwrap_or_default();
                let _ = store.put(marker_key.as_bytes(), &marker_value);
            }

            if new_entities > 0 || new_relations > 0 {
                info!(
                    "Entity extraction: {} new entities, {} new relations from {} chunks",
                    new_entities, new_relations, count
                );
            }

            // Tantivy commit
            {
                let mut idx = tantivy.lock().await;
                let _ = idx.commit();
            }
        }
        Err(e) => {
            warn!(
                "Entity extraction LLM call failed for {} chunks: {}",
                count, e
            );
            // /157 S3: background warn+drop — counted for llm_failures_recent.
            crate::llm_failures::global().record(crate::llm_failures::LlmFailureKind::Ner);
        }
    }

    batch.clear();
    *batch_tokens = 0;
}
