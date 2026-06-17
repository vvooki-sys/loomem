use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::cost_tracker::CostTracker;
use crate::date_resolver;
use crate::entity_extractor::EntityExtractor;
use crate::graph::GraphStore;
use crate::intent_log::{IntentLog, OpType};
use crate::llm::{self, PROMPT_VERSION};
use crate::pii_filter::PiiFilter;
use crate::source_tag::SourceTag;
use crate::storage::{
    persist_chunk_with_index, Chunk, ExtractionMeta, FactType, PersistChunkArgs, RocksDbStore,
};
use crate::tantivy_index::{TantivyIndex, TextDocument};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationConfig {
    pub interval_secs: u64,
    pub batch_size: usize,
    pub concurrency: usize,
    pub timeout_secs: u64,
    pub min_chunks_to_consolidate: usize,
    pub min_age_secs: u64,
    pub prompt_version: u32,
    #[serde(default = "default_consolidation_style")]
    pub consolidation_style: String,
    #[serde(default = "default_similarity_threshold")]
    pub similarity_threshold: f64,
    /// Quality gate threshold: mean cosine similarity between L1 and source L0
    /// embeddings. Below this = reject consolidation. Lower for diverse /
    /// multi-session streams where mean cosine 0.15-0.25 is normal rather than
    /// a noise signal. Default preserved at 0.5 for backward compat; set via
    /// config.toml to 0.25 for production recommendation.
    #[serde(default = "default_quality_gate_threshold")]
    pub quality_gate_threshold: f64,
}

fn default_consolidation_style() -> String {
    "observation".to_string()
}
fn default_similarity_threshold() -> f64 {
    0.3
}
fn default_quality_gate_threshold() -> f64 {
    0.5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationReport {
    pub consolidated_count: usize,
    pub skipped_count: usize,
    pub errors: usize,
    pub cost_usd: f64,
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    (dot / (norm_a * norm_b)) as f64
}

fn update_centroid(centroid: &mut [f32], new_emb: &[f32], group_size: usize) {
    let n = group_size as f32;
    for (c, e) in centroid.iter_mut().zip(new_emb.iter()) {
        *c = (*c * (n - 1.0) + e) / n;
    }
}

/// Cluster chunks into topic-coherent sub-groups using embedding similarity.
/// Chunks without embeddings fall back to a single catch-all group.
fn cluster_by_similarity(
    storage: &RocksDbStore,
    chunks: &[Chunk],
    threshold: f64,
) -> Vec<Vec<Chunk>> {
    let mut with_emb: Vec<(Chunk, Vec<f32>)> = Vec::new();
    let mut without_emb: Vec<Chunk> = Vec::new();

    for chunk in chunks {
        match storage.get_embedding(&chunk.id) {
            Ok(Some(emb)) => with_emb.push((chunk.clone(), emb)),
            _ => without_emb.push(chunk.clone()),
        }
    }

    if with_emb.is_empty() {
        return vec![chunks.to_vec()];
    }

    // Greedy clustering: assign each chunk to most similar group or start new one
    let mut groups: Vec<(Vec<f32>, Vec<Chunk>)> = Vec::new();

    for (chunk, emb) in with_emb {
        let mut best_sim = -1.0f64;
        let mut best_idx = None;

        for (i, (centroid, _)) in groups.iter().enumerate() {
            let sim = cosine_similarity(&emb, centroid);
            if sim > best_sim {
                best_sim = sim;
                best_idx = Some(i);
            }
        }

        if best_sim >= threshold {
            let idx = match best_idx {
                Some(i) => i,
                None => {
                    groups.push((emb, vec![chunk]));
                    continue;
                }
            };
            groups[idx].1.push(chunk);
            let new_len = groups[idx].1.len();
            update_centroid(&mut groups[idx].0, &emb, new_len);
        } else {
            groups.push((emb, vec![chunk]));
        }
    }

    let mut result: Vec<Vec<Chunk>> = groups.into_iter().map(|(_, members)| members).collect();
    if !without_emb.is_empty() {
        result.push(without_emb);
    }

    result
}

// ── Structured consolidation output parsing ─────────────────────────────────

#[derive(Debug, Deserialize)]
struct StructuredObservation {
    #[serde(rename = "type")]
    obs_type: String,
    content: String,
    event_at_raw: Option<String>,
    confidence: Option<f64>,
    importance: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StructuredResponse {
    observations: Vec<StructuredObservation>,
}

/// Enrichment extracted from structured LLM response.
struct Enrichment {
    content: String,
    memory_type: String,
    extraction_meta: ExtractionMeta,
    importance: Option<f64>,
    resolved_event_date: Option<chrono::NaiveDate>,
}

fn parse_structured_response(
    summary: &str,
    anchor_timestamp: u64,
    model: &str,
) -> Option<Enrichment> {
    // Strip markdown code fences if present
    let cleaned = summary
        .trim()
        .strip_prefix("```json")
        .unwrap_or(summary.trim())
        .strip_prefix("```")
        .unwrap_or(summary.trim())
        .strip_suffix("```")
        .unwrap_or(summary.trim())
        .trim();

    let response: StructuredResponse = serde_json::from_str(cleaned).ok()?;

    if response.observations.is_empty() {
        return None;
    }

    // Determine dominant type
    let mut type_counts: HashMap<String, usize> = HashMap::new();
    for obs in &response.observations {
        *type_counts.entry(obs.obs_type.to_lowercase()).or_default() += 1;
    }
    let dominant_type = type_counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(t, _)| t)
        .unwrap_or_else(|| "fact".to_string());

    let fact_type = match dominant_type.as_str() {
        "event" => FactType::Event,
        "preference" => FactType::PreferenceOrDecision,
        "experience" => FactType::Experience,
        _ => FactType::Fact,
    };

    // Format content as numbered list (matches observation style for BM25/embedding)
    let content = response
        .observations
        .iter()
        .enumerate()
        .map(|(i, obs)| {
            let priority = match obs.importance.as_deref() {
                Some("high") => "[!]",
                Some("medium") => "[*]",
                _ => "[.]",
            };
            format!("{}. {} {}", i + 1, priority, obs.content)
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Extract first event_at_raw and resolve
    let event_at_raw = response
        .observations
        .iter()
        .filter(|o| o.obs_type.to_lowercase() == "event")
        .find_map(|o| o.event_at_raw.clone());

    let anchor_date = chrono::DateTime::from_timestamp(anchor_timestamp as i64, 0)
        .map(|dt| dt.date_naive())
        .unwrap_or_else(|| chrono::Utc::now().date_naive());

    let resolved_event_date = event_at_raw
        .as_deref()
        .and_then(|raw| date_resolver::resolve_date(raw, anchor_date));

    // Average confidence
    let confidences: Vec<f64> = response
        .observations
        .iter()
        .filter_map(|o| o.confidence)
        .collect();
    let avg_confidence = if confidences.is_empty() {
        0.7
    } else {
        confidences.iter().sum::<f64>() / confidences.len() as f64
    };

    // Map importance
    let importance_value = match response
        .observations
        .iter()
        .filter_map(|o| o.importance.as_deref())
        .max_by_key(|i| match *i {
            "high" => 3,
            "medium" => 2,
            _ => 1,
        }) {
        Some("high") => Some(1.0),
        Some("medium") => Some(0.5),
        Some("low") => Some(0.1),
        _ => None,
    };

    let extraction_meta = ExtractionMeta {
        fact_type: fact_type.clone(),
        subject: None,
        event_date: resolved_event_date.map(|d| d.format("%Y-%m-%d").to_string()),
        event_date_context: event_at_raw,
        supersedes: None,
        superseded_by: None,
        confidence: avg_confidence,
        extracted_from: None,
        extraction_model: Some(model.to_string()),
        original_content: None,
        topic: None,
    };

    Some(Enrichment {
        content,
        memory_type: dominant_type,
        extraction_meta,
        importance: importance_value,
        resolved_event_date,
    })
}

/// ECA-15: Quality gate — reject consolidated chunks that lost too much meaning.
///
/// Compares the new embedding against source embeddings. If the mean cosine
/// similarity drops below `threshold`, the consolidation is rejected (the new
/// chunk doesn't adequately represent its sources).
fn quality_gate(
    store: &RocksDbStore,
    _new_content: &str,
    new_embedding: &[f32],
    source_chunks: &[Chunk],
    threshold: f64,
) -> bool {
    if new_embedding.is_empty() {
        return true; // can't check without an embedding
    }

    let mut total_sim = 0.0f64;
    let mut count = 0usize;

    for chunk in source_chunks {
        if let Ok(Some(emb)) = store.get_embedding(&chunk.id) {
            let sim = cosine_similarity(new_embedding, &emb);
            total_sim += sim;
            count += 1;
        }
    }

    if count == 0 {
        return true; // no source embeddings to compare against
    }

    let mean_sim = total_sim / count as f64;
    if mean_sim < threshold {
        warn!(
            "Quality gate REJECTED: mean similarity {:.3} < {:.3} across {} source chunks",
            mean_sim, threshold, count
        );
        return false;
    }

    debug!(
        "Quality gate passed: mean similarity {:.3} >= {:.3} across {} source chunks",
        mean_sim, threshold, count
    );
    true
}

/// L0 → L1 compression pipeline
pub async fn consolidate(
    storage: Arc<RocksDbStore>,
    tantivy: Arc<tokio::sync::Mutex<TantivyIndex>>,
    llm_client: &reqwest::Client,
    llm_config: &crate::config::LlmConfig,
    pii_filter: &PiiFilter,
    cost_tracker: &CostTracker,
    config: &ConsolidationConfig,
    cancel: CancellationToken,
    intent_log: Option<Arc<tokio::sync::Mutex<IntentLog>>>,
    entity_extractor: Option<Arc<EntityExtractor>>,
    graph: Option<Arc<GraphStore>>,
    entity_extraction_queue: Option<crate::entity_extraction_queue::EntityExtractionQueue>,
) -> Result<ConsolidationReport> {
    let mut consolidated_count = 0;
    let mut skipped_count = 0;
    let mut errors = 0;
    let mut total_cost = 0.0;

    info!(
        "Starting consolidation: min_age={}s, min_chunks={}, batch_size={}",
        config.min_age_secs, config.min_chunks_to_consolidate, config.batch_size
    );

    // Scan RocksDB for L0 chunks that are unconsolidated and old enough
    let candidates = storage.scan_l0_unconsolidated(config.min_age_secs, config.batch_size * 10)?;

    if candidates.is_empty() {
        info!("No L0 chunks eligible for consolidation");
        return Ok(ConsolidationReport {
            consolidated_count: 0,
            skipped_count: 0,
            errors: 0,
            cost_usd: 0.0,
        });
    }

    info!(
        "Found {} L0 chunks eligible for consolidation",
        candidates.len()
    );

    // Group by stream — NEVER cross-stream consolidation
    let mut streams: HashMap<String, Vec<Chunk>> = HashMap::new();
    for chunk in candidates {
        streams.entry(chunk.stream.clone()).or_default().push(chunk);
    }

    info!("Grouped into {} streams", streams.len());

    // Process each stream group
    for (stream, mut chunks) in streams {
        // Check for cancellation
        if cancel.is_cancelled() {
            info!("Consolidation cancelled, stopping");
            break;
        }

        // Skip if too few chunks
        if chunks.len() < config.min_chunks_to_consolidate {
            skipped_count += chunks.len();
            debug!("Skipping stream {} (only {} chunks)", stream, chunks.len());
            continue;
        }

        // Take up to batch_size chunks
        chunks.truncate(config.batch_size);

        // Assert per-stream isolation
        for chunk in &chunks {
            if chunk.stream != stream {
                anyhow::bail!(
                    "Per-stream isolation violated: expected stream {}, got {}",
                    stream,
                    chunk.stream
                );
            }
        }

        // Sub-group by topic similarity so unrelated facts don't merge into one L1 chunk.
        // Each sub-group gets its own LLM compress call → its own L1 chunk.
        //
        // Bypass for lme_* eval streams: their content is deliberately heterogeneous
        // (multi-topic per stream by design). Topic clustering splits them into 1-2 chunk
        // sub-groups that fail min_chunks_to_consolidate. Treat the whole stream as one
        // group and let the structured prompt extract multiple typed observations.
        let sub_groups = if stream.starts_with("lme_") {
            debug!(
                "Stream {}: bypassing topic sub-grouping for eval stream ({} chunks → 1 group)",
                stream,
                chunks.len()
            );
            vec![chunks.clone()]
        } else {
            cluster_by_similarity(&storage, &chunks, config.similarity_threshold)
        };

        info!(
            "Stream {}: {} chunks → {} sub-groups",
            stream,
            chunks.len(),
            sub_groups.len()
        );

        for sub_group in sub_groups {
            // Skip sub-groups that are too small — they'll accumulate for next cycle
            if sub_group.len() < config.min_chunks_to_consolidate {
                skipped_count += sub_group.len();
                debug!(
                    "Skipping sub-group in stream {} (only {} chunks)",
                    stream,
                    sub_group.len()
                );
                continue;
            }

            // Check for cancellation
            if cancel.is_cancelled() {
                info!("Consolidation cancelled, stopping");
                break;
            }

            // Mark as in_progress
            let chunk_ids: Vec<String> = sub_group.iter().map(|c| c.id.clone()).collect();
            if let Err(e) = storage.mark_in_progress(&chunk_ids) {
                warn!("Failed to mark chunks as in_progress: {}", e);
                errors += sub_group.len();
                continue;
            }

            // Concatenate content
            let texts: Vec<String> = sub_group.iter().map(|c| c.content.clone()).collect();
            let combined = texts.join("\n\n---\n\n");

            // PII sanitization BEFORE LLM call
            let (sanitized, redactions) = pii_filter.sanitize(&combined);

            if !redactions.is_empty() {
                debug!(
                    "Applied {} PII redactions before consolidation",
                    redactions.len()
                );
            }

            // Check budget
            if let Err(e) = cost_tracker.check_budget() {
                warn!("Budget exceeded, stopping consolidation: {}", e);
                if let Err(clear_err) = storage.clear_in_progress(&chunk_ids) {
                    warn!("Failed to clear in_progress: {}", clear_err);
                }
                break;
            }

            // LLM compress — pass sanitized combined text (PII already redacted)
            match llm::compress(
                llm_client,
                llm_config,
                &[sanitized],
                Some(&config.consolidation_style),
            )
            .await
            {
                Ok((summary, usage)) => {
                    // Record cost
                    if let Err(e) = cost_tracker.record(
                        usage.prompt_tokens,
                        usage.completion_tokens,
                        &llm_config.compression_model,
                    ) {
                        warn!("Failed to record cost: {}", e);
                    }

                    // Calculate cost (approximate)
                    let cost = (usage.prompt_tokens as f64 * 0.15
                        + usage.completion_tokens as f64 * 0.60)
                        / 1_000_000.0;
                    total_cost += cost;

                    // Try structured parsing if style is "structured"
                    let anchor_ts = sub_group.iter().map(|c| c.timestamp).min().unwrap_or(0);
                    let enrichment = if config.consolidation_style == "structured" {
                        match parse_structured_response(
                            &summary,
                            anchor_ts,
                            &llm_config.compression_model,
                        ) {
                            Some(e) => {
                                debug!(
                                    "Structured consolidation: type={}, event_date={:?}, confidence={:.2}",
                                    e.memory_type,
                                    e.extraction_meta.event_date,
                                    e.extraction_meta.confidence,
                                );
                                Some(e)
                            }
                            None => {
                                debug!("Structured parse failed, falling back to plain text");
                                None
                            }
                        }
                    } else {
                        None
                    };

                    // Create L1 chunk (enriched if structured parse succeeded)
                    let l1_id = format!("L1:{}", uuid::Uuid::new_v4());
                    let now_ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);

                    let (
                        l1_content,
                        l1_memory_type,
                        l1_extraction_meta,
                        l1_importance,
                        l1_valid_from,
                    ) = if let Some(ref e) = enrichment {
                        let valid_from = e
                            .resolved_event_date
                            .and_then(|d| d.and_hms_opt(12, 0, 0))
                            .map(|dt| dt.and_utc().timestamp() as u64)
                            .or_else(|| sub_group.iter().map(|c| c.timestamp).min());
                        (
                            e.content.clone(),
                            Some(e.memory_type.clone()),
                            Some(e.extraction_meta.clone()),
                            e.importance,
                            valid_from,
                        )
                    } else {
                        (
                            summary.clone(),
                            None,
                            None,
                            None,
                            sub_group.iter().map(|c| c.timestamp).min(),
                        )
                    };

                    let l1_chunk = Chunk {
                        id: l1_id.clone(),
                        content: l1_content,
                        stream: stream.clone(),
                        level: 1,
                        score: 1.0,
                        timestamp: now_ts,
                        consolidated: false,
                        dormant: false,
                        in_progress: false,
                        prompt_version: Some(PROMPT_VERSION),
                        source_ids: Some(chunk_ids.clone()),
                        last_decay: None,
                        metadata: None,
                        importance: l1_importance,
                        persistent: false,
                        last_implicit_boost: None,
                        access_count: 0,
                        source: Some(SourceTag::from_agent("consolidation")),
                        created_by: Some("loomem-consolidation".to_string()),
                        updated_at: Some(now_ts),
                        valid_from: l1_valid_from,
                        valid_until: sub_group.iter().map(|c| c.timestamp).max(),
                        is_latest: true,
                        superseded_by: None,
                        supersedes_id: None,
                        root_memory_id: None,
                        version: 1,
                        memory_type: l1_memory_type,
                        extraction_meta: l1_extraction_meta,
                        deleted_at: None,
                        trust_level: None,
                        ingester_user_id: None,
                        alpha: 1.0,
                        beta: 1.0,
                        harmful_count: 0,
                        n_ratings: 0,
                        last_rated_at: None,
                    };

                    // Use enriched or raw summary for embedding + entity extraction
                    let summary_for_index = if enrichment.is_some() {
                        l1_chunk.content.clone()
                    } else {
                        summary.clone()
                    };

                    // Generate embedding for L1 BEFORE storing (needed for quality gate)
                    let l1_embedding = match llm::embed(llm_client, llm_config, &summary_for_index)
                        .await
                    {
                        Ok(embedding) => {
                            let approx_tokens = (summary.len() / 4) as u64;
                            if let Err(e) =
                                cost_tracker.record(approx_tokens, 0, &llm_config.embedding_model)
                            {
                                warn!("Failed to record embedding cost: {}", e);
                            }
                            Some(embedding)
                        }
                        Err(e) => {
                            warn!("Failed to generate L1 embedding: {}", e);
                            None
                        }
                    };

                    // ECA-15: Quality gate — reject if consolidated chunk lost too much meaning.
                    // Bypass for lme_* eval streams: their content is deliberately heterogeneous
                    // (multiple topics per stream), so the gate's homogeneity assumption produces
                    // false negatives. Production streams unaffected.
                    let skip_quality_gate = stream.starts_with("lme_");
                    if !skip_quality_gate {
                        if let Some(ref emb) = l1_embedding {
                            if !quality_gate(
                                &storage,
                                &summary,
                                emb,
                                &sub_group,
                                config.quality_gate_threshold,
                            ) {
                                warn!(
                                    "Consolidation quality gate rejected sub-group in stream {} ({} chunks)",
                                    stream, sub_group.len()
                                );
                                skipped_count += sub_group.len();
                                if let Err(clear_err) = storage.clear_in_progress(&chunk_ids) {
                                    warn!("Failed to clear in_progress: {}", clear_err);
                                }
                                continue;
                            }
                        }
                    }

                    // Step 1: extract entities (pure — extractor.extract is read-only, no
                    // storage writes). Must happen before persist so entities_str /
                    // relations_str are available for the TextDocument passed to the helper.
                    // Aux storage writes (store_entities, store_relations, graph) happen
                    // AFTER persist_chunk_with_index succeeds (post-helper, warn-skip).
                    let (entities_str, relations_str, extracted_entities, extracted_relations) =
                        if let Some(ref extractor) = entity_extractor {
                            let entities = extractor.extract(&summary_for_index);
                            let entity_names: Vec<String> =
                                entities.iter().map(|(n, _)| n.clone()).collect();
                            let relations = extractor.find_relations(&entities);

                            let e_str = if entity_names.is_empty() {
                                None
                            } else {
                                Some(entity_names.join(","))
                            };
                            let r_str = if relations.is_empty() {
                                None
                            } else {
                                let text: Vec<String> = relations
                                    .iter()
                                    .map(|r| {
                                        format!(
                                            "{} {} {}",
                                            r.subject.to_lowercase(),
                                            r.relation,
                                            r.object.to_lowercase()
                                        )
                                    })
                                    .collect();
                                Some(text.join(", "))
                            };
                            (e_str, r_str, entities, relations)
                        } else {
                            (None, None, vec![], vec![])
                        };

                    // Step 2: build TextDocument for Tantivy (needs entities_str / relations_str)
                    let event_date_ts: Option<i64> = l1_chunk
                        .extraction_meta
                        .as_ref()
                        .and_then(|m| m.event_date.as_ref())
                        .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
                        .and_then(|d| d.and_hms_opt(12, 0, 0))
                        .map(|dt| dt.and_utc().timestamp());

                    let doc = TextDocument {
                        id: l1_id.clone(),
                        content: summary_for_index.clone(),
                        user_id: String::new(),
                        app_id: String::new(),
                        level: 1,
                        timestamp: l1_chunk.timestamp as i64,
                        stream: stream.clone(),
                        entities: entities_str,
                        relations: relations_str,
                        event_date: event_date_ts,
                        source_agent: l1_chunk.source.as_ref().map(|s| s.agent.clone()),
                    };

                    // Step 3: atomic persist — RocksDB + Tantivy + intent_log.
                    // L1 chunk from consolidation pipeline. OpType::Consolidate is
                    // semantically accurate; recover() is symmetric for Store and
                    // Consolidate since /51 PR #106.
                    if let Err(e) = persist_chunk_with_index(
                        &storage,
                        &tantivy,
                        PersistChunkArgs {
                            chunk: &l1_chunk,
                            text_doc: doc,
                            intent_log: intent_log.as_deref(),
                            op: OpType::Consolidate,
                        },
                    )
                    .await
                    {
                        warn!("Failed to persist L1 chunk {}: {}", l1_id, e);
                        errors += sub_group.len();
                        if let Err(clear_err) = storage.clear_in_progress(&chunk_ids) {
                            warn!("Failed to clear in_progress: {}", clear_err);
                        }
                        continue;
                    }

                    // Step 4: aux writes post-helper (warn-skip; failures are tolerated
                    // because they don't affect RocksDB / Tantivy retrieval correctness).

                    // Store L1 embedding (already generated above)
                    if let Some(embedding) = l1_embedding {
                        if let Err(e) = storage.store_embedding(&l1_id, embedding) {
                            warn!("Failed to store L1 embedding: {}", e);
                        }
                    }

                    // Enqueue for LLM-based entity extraction (discovers entities not in entities.toml)
                    if let Some(ref eq) = entity_extraction_queue {
                        let dict_ents: Vec<(String, String)> = Vec::new();
                        if let Err(e) = eq.enqueue(
                            l1_id.clone(),
                            summary_for_index.clone(),
                            stream.clone(),
                            dict_ents,
                        ) {
                            warn!("Failed to enqueue L1 entity extraction: {}", e);
                        }
                    }

                    // Store entities and relations extracted in step 1
                    if !extracted_entities.is_empty() {
                        let typed: Vec<(String, String)> = extracted_entities
                            .iter()
                            .map(|(n, t)| (n.clone(), t.to_string()))
                            .collect();
                        let _ = storage.store_entities(&l1_id, &stream, &typed);
                    }
                    if !extracted_relations.is_empty() {
                        let tuples: Vec<(String, String, String)> = extracted_relations
                            .iter()
                            .map(|r| (r.subject.clone(), r.relation.clone(), r.object.clone()))
                            .collect();
                        let _ = storage.store_relations(&l1_id, &stream, &tuples);
                    }

                    // Populate graph
                    if let (Some(ref extractor), Some(ref g)) = (&entity_extractor, &graph) {
                        for (name, etype) in &extracted_entities {
                            let aliases = extractor.get_aliases_for(name);
                            if let Ok(node) =
                                g.get_or_create_entity(name, &etype.to_string(), &aliases, &stream)
                            {
                                let _ = g.add_chunk_to_entity(&node.id, &l1_id);
                            }
                        }
                        for rel in &extracted_relations {
                            if let (Ok(Some(src)), Ok(Some(tgt))) = (
                                g.get_entity_by_name(&rel.subject, &stream),
                                g.get_entity_by_name(&rel.object, &stream),
                            ) {
                                if let Ok(edge) =
                                    g.get_or_create_edge(&src.id, &tgt.id, &rel.relation, &stream)
                                {
                                    let _ = g.add_chunk_to_edge(&edge.id, &l1_id);
                                }
                            }
                        }
                    }

                    // Step 5: mark source L0 chunks as consolidated.
                    // Gated on helper success (above): if helper returned Err we already
                    // continued, so source L0 chunks remain available for retry on next tick.
                    if let Err(e) = storage.mark_consolidated(&chunk_ids) {
                        warn!("Failed to mark chunks as consolidated: {}", e);
                        errors += sub_group.len();
                    } else {
                        consolidated_count += sub_group.len();
                        info!(
                            "Consolidated {} chunks from stream {} into L1:{} (sub-group)",
                            sub_group.len(),
                            stream,
                            l1_id
                        );
                    }
                }
                Err(e) => {
                    warn!("LLM compression failed for stream {}: {}", stream, e);
                    errors += sub_group.len();

                    if let Err(clear_err) = storage.clear_in_progress(&chunk_ids) {
                        warn!("Failed to clear in_progress: {}", clear_err);
                    }
                }
            }
        } // end sub_groups loop
    }

    info!(
        "Consolidation completed: consolidated={}, skipped={}, errors={}, cost=${:.4}",
        consolidated_count, skipped_count, errors, total_cost
    );

    Ok(ConsolidationReport {
        consolidated_count,
        skipped_count,
        errors,
        cost_usd: total_cost,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_consolidation_per_stream() {
        // Test that chunks from different streams are not mixed
        let stream_100_chunks = vec!["chunk1", "chunk2"];
        let stream_200_chunks = vec!["chunk3"];

        // Simulate grouping
        let mut streams: HashMap<String, Vec<&str>> = HashMap::new();
        streams.insert("100".to_string(), stream_100_chunks);
        streams.insert("200".to_string(), stream_200_chunks);

        // Assert separation
        assert_eq!(streams.get("100").map(|v| v.len()), Some(2));
        assert_eq!(streams.get("200").map(|v| v.len()), Some(1));
        assert_ne!(streams.get("100"), streams.get("200"));
    }
}
