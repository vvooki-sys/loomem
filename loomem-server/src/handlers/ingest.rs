use axum::{extract::State, http::StatusCode, Json};
use chrono::Utc;
use loomem_core::intent_log::OpType;
use loomem_core::source_tag::SourceTag;
use loomem_core::storage::{persist_chunk_with_index, Chunk, PersistChunkArgs};
use loomem_core::{embeddings, TextDocument};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::warn;

use super::types::{RetagAllResponse, StoreRequest, StoreResponse};
use super::AppError;
use crate::auth::{self, AuthContext};
use crate::AppState;

fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    (dot / (norm_a * norm_b)) as f64
}

/// Shared persistence logic: RocksDB store → legacy event key → entity extraction →
/// graph population → Tantivy index → intent log commit → embedding queue →
/// entity extraction queue → cache invalidation → audit log.
pub async fn persist_chunk(
    state: &Arc<AppState>,
    chunk: Chunk,
    content: &str,
    user_id: &str,
    app_id: &str,
    stream: &str,
    level: i32,
    timestamp: i64,
    stream_id: Option<&str>,
    metadata: Option<&serde_json::Value>,
) -> Result<String, AppError> {
    // Pre-ingestion sanitization: HTML strip + injection detection
    let sanitize_result = loomem_core::sanitizer::sanitize(content);
    let content = &sanitize_result.content;
    let chunk = if sanitize_result.html_stripped || sanitize_result.injection_detected {
        let mut c = chunk;
        c.content = sanitize_result.content.clone();
        c
    } else {
        chunk
    };

    // PII sanitization at ingest — redact before any storage
    let (sanitized_content, pii_redactions) = state.pii_filter.sanitize(content);
    let content = &sanitized_content;
    let chunk = if !pii_redactions.is_empty() {
        let mut c = chunk;
        c.content = sanitized_content.clone();
        tracing::info!(
            "PII: redacted {} items at ingest for chunk {}",
            pii_redactions.len(),
            c.id
        );
        c
    } else {
        chunk
    };

    let id = chunk.id.clone();

    // Mark profile dirty for this stream (cache invalidation)
    let _ = loomem_core::profile::mark_profile_dirty(&state.store, stream);
    // cycle/139: invalidate the manifest cache only for shared/project streams;
    // private streams have no manifest so the dirty key would never be consumed.
    if loomem_core::manifest::classify_stream(stream) != loomem_core::manifest::StreamKind::Private
    {
        let _ = loomem_core::manifest::mark_manifest_dirty(&state.store, stream);
    }

    // Also store legacy event key for backward compatibility
    let key = format!("event:{}", id);
    let value = json!({
        "id": id,
        "content": content,
        "user_id": user_id,
        "app_id": app_id,
        "level": level,
        "timestamp": timestamp,
        "stream": stream,
        "stream_id": stream_id,
        "metadata": metadata,
    });
    let value_bytes = serde_json::to_vec(&value)?;
    state.store.put(key.as_bytes(), &value_bytes)?;

    // Extract entities
    let entities = state.entity_extractor.extract(content);
    let entity_names: Vec<String> = entities.iter().map(|(name, _)| name.clone()).collect();
    let entities_str = if !entity_names.is_empty() {
        Some(entity_names.join(","))
    } else {
        None
    };

    // Find relations for extracted entities
    let relations = state.entity_extractor.find_relations(&entities);
    let relations_str = if !relations.is_empty() {
        let rel_text: Vec<String> = relations
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
        Some(rel_text.join(", "))
    } else {
        None
    };

    // Store entities with types in RocksDB
    if !entities.is_empty() {
        let typed_entities: Vec<(String, String)> = entities
            .iter()
            .map(|(name, etype)| (name.clone(), etype.to_string()))
            .collect();
        state.store.store_entities(&id, stream, &typed_entities)?;
    }

    // Store relations in RocksDB
    if !relations.is_empty() {
        let rel_tuples: Vec<(String, String, String)> = relations
            .iter()
            .map(|r| (r.subject.clone(), r.relation.clone(), r.object.clone()))
            .collect();
        state.store.store_relations(&id, stream, &rel_tuples)?;
    }

    // Populate knowledge graph (stream-scoped)
    for (entity_name, entity_type) in &entities {
        let aliases = state.entity_extractor.get_aliases_for(entity_name);
        match state.graph.get_or_create_entity(
            entity_name,
            &entity_type.to_string(),
            &aliases,
            stream,
        ) {
            Ok(node) => {
                if let Err(e) = state.graph.add_chunk_to_entity(&node.id, &id) {
                    warn!("Failed to link chunk to graph entity: {}", e);
                }
            }
            Err(e) => warn!("Failed to create graph entity: {}", e),
        }
    }
    for rel in &relations {
        if let (Ok(Some(src)), Ok(Some(tgt))) = (
            state.graph.get_entity_by_name(&rel.subject, stream),
            state.graph.get_entity_by_name(&rel.object, stream),
        ) {
            match state
                .graph
                .get_or_create_edge(&src.id, &tgt.id, &rel.relation, stream)
            {
                Ok(edge) => {
                    let _ = state.graph.add_chunk_to_edge(&edge.id, &id);
                }
                Err(e) => warn!("Failed to create graph edge: {}", e),
            }
        }
    }

    // Parse event_date from extraction_meta for Tantivy temporal indexing
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

    // Build TextDocument for Tantivy
    let doc = TextDocument {
        id: id.clone(),
        content: content.to_string(),
        user_id: user_id.to_string(),
        app_id: app_id.to_string(),
        level,
        timestamp,
        stream: stream.to_string(),
        entities: entities_str,
        relations: relations_str,
        event_date: event_date_ts,
        source_agent: chunk.source.as_ref().map(|s| s.agent.clone()),
    };

    // CS1: atomic RocksDB + Tantivy persist via helper
    persist_chunk_with_index(
        &state.store,
        &state.tantivy,
        PersistChunkArgs {
            chunk: &chunk,
            text_doc: doc,
            intent_log: state.intent_log.as_deref(),
            op: OpType::Store,
        },
    )
    .await?;

    // Enqueue embedding for background batch processing
    if let Some(ref queue) = state.embedding_queue {
        if let Err(e) = queue.enqueue(id.clone(), content.to_string()) {
            warn!("Failed to enqueue embedding for {}: {}", id, e);
        }
    }

    // Enqueue LLM entity extraction (discovers entities not in entities.toml)
    if let Some(ref eq) = state.entity_extraction_queue {
        let dict_ents: Vec<(String, String)> = entities
            .iter()
            .map(|(name, etype)| (name.clone(), etype.to_string()))
            .collect();
        if let Err(e) = eq.enqueue(
            id.clone(),
            content.to_string(),
            stream.to_string(),
            dict_ents,
        ) {
            warn!("Failed to enqueue entity extraction for {}: {}", id, e);
        }
    }

    tracing::debug!("Stored event: id={}", id);

    // Invalidate search cache
    {
        let mut cache = state.query_cache.lock().await;
        cache.clear();
    }

    // Audit log
    {
        let ns_name = state
            .config
            .namespaces
            .iter()
            .find(|(_, v)| v.as_str() == stream)
            .map(|(k, _)| k.as_str())
            .unwrap_or("unknown");
        let audit_dir = std::path::Path::new("memory/audit");
        let _ = std::fs::create_dir_all(audit_dir);
        let date_str = Utc::now().format("%Y-%m-%d").to_string();
        let audit_path = audit_dir.join(format!("{}.jsonl", date_str));
        let audit_line = json!({
            "ts": Utc::now().to_rfc3339(),
            "op": "store",
            "ns": ns_name,
            "stream": stream,
            "agent": user_id,
            "id": &id,
            "len": content.len(),
        });
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&audit_path)
        {
            use std::io::Write;
            let _ = writeln!(f, "{}", audit_line);
        }
    }

    Ok(id)
}

/// /142 + /143: classify a chunk's content *form* via the LLM (the sole
/// classifier since /143) and persist it to the sidecar keyspace
/// (`content_type:<id>`). Best-effort and additive — never touches the chunk
/// row, never fails the ingest. Writes nothing when typing is disabled or the
/// LLM call fails (`classify_content` returns `None`).
async fn classify_and_store_content_type(state: &Arc<AppState>, chunk_id: &str, content: &str) {
    let classifier = crate::content_type::HttpContentTypeClassifier::new(
        state.http_client.clone(),
        &state.config.llm,
        state.config.content_type.model.clone(),
    );
    if let Some(meta) = loomem_core::content_type::classify_content(
        &classifier,
        &state.config.content_type,
        &state.store,
        content,
    )
    .await
    {
        loomem_core::content_type::put_content_type(&state.store, chunk_id, &meta);
    }
}

pub async fn store_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Json(payload): Json<StoreRequest>,
) -> Result<Json<StoreResponse>, AppError> {
    let id = uuid::Uuid::new_v4().to_string();
    let timestamp = Utc::now().timestamp();

    let user_id = payload.user_id.unwrap_or_else(|| "default".to_string());
    let app_id = payload.app_id.unwrap_or_else(|| "default".to_string());
    let level = payload.level.unwrap_or(0);

    // Redundant with the MCP dispatcher gate, but REST /v1/store is a
    // separate entry point and must enforce §D5 on its own: Reader may not
    // write on shared scope. Private scope = owner, no gate needed.
    if auth.scope == crate::auth::KeyScope::Shared && !auth.role.can_write() {
        return Err(AppError::Forbidden(
            "Read-only access: write operations not permitted for Reader role on shared scope."
                .into(),
        ));
    }

    let stream = auth::validate_stream(&auth, payload.stream.as_deref())
        .map_err(|_| AppError::BadRequest("Access denied: cannot write to this stream".into()))?;

    // Content size limits
    const MAX_CONTENT_BYTES: usize = 102_400; // 100 KB
    const MAX_METADATA_BYTES: usize = 10_240; // 10 KB
    if payload.content.len() > MAX_CONTENT_BYTES {
        return Err(AppError::BadRequest(format!(
            "Content too large: {} bytes (max {})",
            payload.content.len(),
            MAX_CONTENT_BYTES
        )));
    }
    if let Some(ref meta) = payload.metadata {
        let meta_size = serde_json::to_string(meta).map(|s| s.len()).unwrap_or(0);
        if meta_size > MAX_METADATA_BYTES {
            return Err(AppError::BadRequest(format!(
                "Metadata too large: {} bytes (max {})",
                meta_size, MAX_METADATA_BYTES
            )));
        }
    }

    // Calculate surprise score (importance) via embedding similarity
    // Also capture the embedding for contradiction detection
    let (importance, new_embedding_opt) = if state.config.storage.vector_enabled {
        let embed_result = if let Some(ref embedder) = state.local_embedder {
            embedder.embed(&payload.content)
        } else if let Some(api_key) = state.config.llm.get_api_key() {
            embeddings::embed(
                &state.http_client,
                &api_key,
                &state.config.llm.embedding_model,
                &payload.content,
            )
            .await
        } else {
            Err(anyhow::anyhow!("No embedding provider"))
        };
        {
            match embed_result {
                Ok(new_embedding) => {
                    let imp = match state.store.get_all_embeddings() {
                        Ok(all_embeddings) => {
                            if !all_embeddings.is_empty() {
                                let mut similarities: Vec<f64> = all_embeddings
                                    .iter()
                                    .map(|(_, vec)| cosine_similarity(&new_embedding, vec))
                                    .collect();
                                similarities.sort_by(|a, b| {
                                    b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal)
                                });

                                let max_similarity = similarities
                                    .into_iter()
                                    .take(5)
                                    .max_by(|a, b| {
                                        a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
                                    })
                                    .unwrap_or(0.0);
                                let surprise = 1.0 - max_similarity;

                                let importance_value = if surprise
                                    > state.config.search.importance.high_threshold
                                {
                                    state.config.search.importance.high_weight
                                } else if surprise > state.config.search.importance.low_threshold {
                                    state.config.search.importance.medium_weight
                                } else {
                                    state.config.search.importance.low_weight
                                };

                                tracing::debug!(
                                    "Surprise score for {}: {:.3} -> importance: {:.1}",
                                    id,
                                    surprise,
                                    importance_value
                                );
                                Some(importance_value)
                            } else {
                                None
                            }
                        }
                        Err(e) => {
                            warn!("Failed to get embeddings for surprise scoring: {}", e);
                            None
                        }
                    };
                    (imp, Some(new_embedding))
                }
                Err(e) => {
                    warn!("Failed to generate embedding for surprise scoring: {}", e);
                    (None, None)
                }
            }
        }
    } else {
        (None, None)
    };

    // Allow explicit importance override (e.g. from auto_improve)
    let importance = payload.importance.or(importance);

    // Build SourceTag: prefer structured fields, fall back to legacy `source` string
    let source_tag: Option<SourceTag> = if let Some(ref agent) = payload.source_agent {
        Some(SourceTag {
            agent: agent.clone(),
            session: payload.source_session.clone(),
            channel: payload.source_channel.clone(),
        })
    } else {
        payload
            .source
            .as_deref()
            .map(SourceTag::from_agent)
            .or_else(|| Some(SourceTag::from_agent("api")))
    };

    let chunk = Chunk {
        id: id.clone(),
        content: payload.content.clone(),
        stream: stream.clone(),
        level,
        score: 1.0,
        timestamp: timestamp as u64,
        consolidated: false,
        dormant: false,
        in_progress: false,
        prompt_version: None,
        source_ids: None,
        last_decay: None,
        metadata: payload.metadata.clone(),
        importance,
        persistent: payload.persistent.unwrap_or(false),
        last_implicit_boost: None,
        access_count: 0,
        source: source_tag.clone(),
        created_by: payload
            .metadata
            .as_ref()
            .and_then(|m| m.get("created_by"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        updated_at: Some(timestamp as u64),
        valid_from: Some(payload.valid_from.unwrap_or(timestamp as u64)),
        valid_until: payload.valid_until,
        is_latest: true,
        superseded_by: None,
        supersedes_id: None,
        root_memory_id: None,
        version: 1,
        memory_type: None,
        extraction_meta: None,
        deleted_at: None,
        trust_level: Some(loomem_core::storage::derive_trust_level(
            source_tag.as_ref().map(|s| s.agent.as_str()),
        )),
        ingester_user_id: auth.user_id.clone(),
        alpha: 1.0,
        beta: 1.0,
        harmful_count: 0,
        n_ratings: 0,
        last_rated_at: None,
    };

    // Contradiction detection: check if new chunk updates/extends existing memory
    let chunk = if state.config.contradiction.enabled {
        if let Some(ref new_emb) = new_embedding_opt {
            match loomem_core::contradiction::find_candidates(
                &state.store,
                new_emb,
                &stream,
                &state.config.contradiction,
            ) {
                Ok(candidates) if !candidates.is_empty() => {
                    let mut result_chunk = chunk;
                    for candidate in &candidates {
                        match loomem_core::contradiction::classify_relation(
                            &state.http_client,
                            &state.config.llm,
                            &state.config.contradiction.model,
                            &candidate.chunk.content,
                            &result_chunk.content,
                        )
                        .await
                        {
                            Ok(classification) => {
                                match classification.relation.as_str() {
                                    "updates" => {
                                        tracing::info!(
                                            "Contradiction detected: {} supersedes {} ({})",
                                            result_chunk.id,
                                            candidate.chunk.id,
                                            classification.reason
                                        );
                                        match loomem_core::contradiction::apply_supersede(
                                            &state.store,
                                            &candidate.chunk,
                                            result_chunk.clone(),
                                        ) {
                                            Ok(updated) => {
                                                result_chunk = updated;
                                                break; // only supersede one chunk
                                            }
                                            Err(e) => {
                                                warn!("Failed to apply supersede: {}", e);
                                            }
                                        }
                                    }
                                    "extends" => {
                                        tracing::debug!(
                                            "Extension detected: {} extends {} ({})",
                                            result_chunk.id,
                                            candidate.chunk.id,
                                            classification.reason
                                        );
                                        result_chunk = loomem_core::contradiction::apply_extend(
                                            &candidate.chunk,
                                            result_chunk,
                                        );
                                        break; // link to first match only
                                    }
                                    _ => {} // "none" — continue checking others
                                }
                            }
                            Err(e) => {
                                warn!("Contradiction classification failed: {}", e);
                            }
                        }
                    }
                    result_chunk
                }
                Ok(_) => chunk, // no candidates
                Err(e) => {
                    warn!("Contradiction candidate search failed: {}", e);
                    chunk
                }
            }
        } else {
            chunk // no embedding available
        }
    } else {
        chunk // contradiction disabled
    };

    persist_chunk(
        &state,
        chunk,
        &payload.content,
        &user_id,
        &app_id,
        &stream,
        level,
        timestamp,
        payload.stream_id.as_deref(),
        payload.metadata.as_ref(),
    )
    .await?;

    // /142: content-type classification → sidecar (additive, best-effort).
    // Chunk row is untouched; det runs always, LLM only when enabled + ambiguous.
    classify_and_store_content_type(&state, &id, &payload.content).await;

    // Emit store event
    if let Some(ref tx) = state.event_tx {
        loomem_core::event_log::emit(
            tx,
            loomem_core::event_log::MemoryEvent::Store {
                content_len: payload.content.len(),
                chunk_count: 1,
                stream_id: auth.stream_id.clone(),
                source: source_tag
                    .as_ref()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "api".to_string()),
            },
        );
    }

    // /150e-2 access-audit (ADR-018): record the write. No-op when disabled.
    crate::access_hook::record_access(
        &state,
        &auth,
        loomem_core::access_audit::AccessOp::Store,
        Some(&id),
        1,
    );

    Ok(Json(StoreResponse {
        id,
        status: "stored".to_string(),
    }))
}

pub async fn embed_missing_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !state.config.storage.vector_enabled {
        return Ok(Json(
            serde_json::json!({"status": "skipped", "reason": "vector_enabled=false"}),
        ));
    }
    let api_key = state
        .config
        .llm
        .get_api_key()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

    let all_chunks = state
        .store
        .get_all_chunks()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let existing_embeddings = state
        .store
        .get_all_embeddings()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let embedded_ids: std::collections::HashSet<String> =
        existing_embeddings.into_iter().map(|(id, _)| id).collect();

    let missing: Vec<_> = all_chunks
        .iter()
        .filter(|c| !embedded_ids.contains(&c.id))
        .collect();
    let total_missing = missing.len();
    tracing::info!("embed-missing: {} chunks need embeddings", total_missing);

    let mut embedded = 0u32;
    let mut failed = 0u32;
    for chunk in missing {
        match embeddings::embed(
            &state.http_client,
            &api_key,
            &state.config.llm.embedding_model,
            &chunk.content,
        )
        .await
        {
            Ok(embedding) => {
                if state.store.store_embedding(&chunk.id, embedding).is_ok() {
                    embedded += 1;
                } else {
                    failed += 1;
                }
            }
            Err(e) => {
                tracing::warn!("embed-missing: failed for {}: {}", chunk.id, e);
                failed += 1;
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
        }
        if embedded.is_multiple_of(50) && embedded > 0 {
            tracing::info!("embed-missing: progress {}/{}", embedded, total_missing);
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }

    tracing::info!(
        "embed-missing complete: {} embedded, {} failed",
        embedded,
        failed
    );
    Ok(Json(serde_json::json!({
        "status": "done",
        "total_missing": total_missing,
        "embedded": embedded,
        "failed": failed,
    })))
}

pub async fn retag_all_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<RetagAllResponse>, AppError> {
    tracing::info!("Starting retag-all operation");

    let chunks = state.store.get_all_chunks()?;
    tracing::info!("Found {} chunks to retag", chunks.len());

    let mut retagged_count = 0;

    for chunk in chunks {
        let entities = state.entity_extractor.extract(&chunk.content);
        let entity_names: Vec<String> = entities.iter().map(|(name, _)| name.clone()).collect();

        let relations = state.entity_extractor.find_relations(&entities);

        // Persist re-extracted entities/relations only when present.
        if !entity_names.is_empty() || !relations.is_empty() {
            if !entities.is_empty() {
                let typed_entities: Vec<(String, String)> = entities
                    .iter()
                    .map(|(name, etype)| (name.clone(), etype.to_string()))
                    .collect();
                state
                    .store
                    .store_entities(&chunk.id, &chunk.stream, &typed_entities)?;
            }

            if !relations.is_empty() {
                let rel_tuples: Vec<(String, String, String)> = relations
                    .iter()
                    .map(|r| (r.subject.clone(), r.relation.clone(), r.object.clone()))
                    .collect();
                state
                    .store
                    .store_relations(&chunk.id, &chunk.stream, &rel_tuples)?;
            }
        }

        // Parity with persist_chunk: re-index EVERY chunk into Tantivy (refreshing
        // source_agent), regardless of entity/relation count. The previous entity
        // gate silently skipped entity-less chunks, violating the
        // persist_chunk_parity invariant. See source-provenance-fixes-brief Issue 2.
        let entities_str = if !entity_names.is_empty() {
            Some(entity_names.join(","))
        } else {
            None
        };

        let relations_str = if !relations.is_empty() {
            let rel_text: Vec<String> = relations
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
            Some(rel_text.join(", "))
        } else {
            None
        };

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
            user_id: "default".to_string(),
            app_id: "default".to_string(),
            level: chunk.level,
            timestamp: chunk.timestamp as i64,
            stream: chunk.stream.clone(),
            entities: entities_str,
            relations: relations_str,
            event_date: event_date_ts,
            source_agent: chunk.source.as_ref().map(|s| s.agent.clone()),
        };

        let mut tantivy = state.tantivy.lock().await;
        tantivy.upsert_document(doc)?;
        drop(tantivy);

        // retagged_count now counts every re-indexed chunk (== total chunk
        // count), giving the parity guarantee the brief AC asks for.
        retagged_count += 1;
    }

    let mut tantivy = state.tantivy.lock().await;
    tantivy.commit()?;
    drop(tantivy);

    tracing::info!("Retag-all complete: {} chunks reindexed", retagged_count);

    Ok(Json(RetagAllResponse {
        status: "completed".to_string(),
        retagged_count,
    }))
}

pub async fn score_all_handler(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !state.config.storage.vector_enabled {
        return Ok(Json(
            json!({"status": "skipped", "reason": "vector_enabled=false"}),
        ));
    }
    // Guard: require API key configured even though scoring uses existing embeddings
    if state.config.llm.get_api_key().is_none() {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    let all_chunks = state
        .store
        .get_all_chunks()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let all_embeddings = state
        .store
        .get_all_embeddings()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let emb_map: HashMap<String, Vec<f32>> = all_embeddings.into_iter().collect();
    let emb_vecs: Vec<(&String, &Vec<f32>)> = emb_map.iter().collect();

    let total = all_chunks.len();
    let mut scored = 0u32;
    let mut skipped = 0u32;
    let mut already_scored = 0u32;

    tracing::info!(
        "score-all: starting batch importance scoring for {} chunks ({} have embeddings)",
        total,
        emb_map.len()
    );

    for chunk in &all_chunks {
        if chunk.importance.is_some() {
            already_scored += 1;
            continue;
        }

        let chunk_emb = match emb_map.get(&chunk.id) {
            Some(e) => e,
            None => {
                skipped += 1;
                continue;
            }
        };

        let max_similarity = emb_vecs
            .iter()
            .filter(|(id, _)| **id != chunk.id)
            .map(|(_, vec)| cosine_similarity(chunk_emb, vec))
            .fold(0.0_f64, |a, b| a.max(b));

        let surprise = 1.0 - max_similarity;
        let importance_value = if surprise > state.config.search.importance.high_threshold {
            state.config.search.importance.high_weight
        } else if surprise > state.config.search.importance.low_threshold {
            state.config.search.importance.medium_weight
        } else {
            state.config.search.importance.low_weight
        };

        let mut updated = chunk.clone();
        updated.importance = Some(importance_value);
        if let Err(e) = state.store.store_chunk(&updated) {
            tracing::warn!("score-all: failed to update {}: {}", chunk.id, e);
        } else {
            scored += 1;
        }

        if scored.is_multiple_of(500) && scored > 0 {
            tracing::info!("score-all: progress {}/{}", scored, total);
        }
    }

    tracing::info!(
        "score-all complete: scored={}, skipped={}, already_scored={}",
        scored,
        skipped,
        already_scored
    );
    Ok(Json(json!({
        "status": "done",
        "total": total,
        "scored": scored,
        "skipped_no_embedding": skipped,
        "already_scored": already_scored,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use loomem_core::storage::DEFAULT_STREAM_ID;

    /// Minimal entity-less chunk. The test harness loads an empty entity
    /// extractor, so any content is entity-less — the exact case the old
    /// `retag_all_handler` entity gate dropped.
    fn entity_less_chunk(id: &str, content: &str, agent: &str) -> Chunk {
        Chunk {
            id: id.to_string(),
            content: content.to_string(),
            stream: DEFAULT_STREAM_ID.to_string(),
            level: 0,
            score: 1.0,
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
            source: Some(SourceTag::from_agent(agent)),
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
        }
    }

    /// Issue 2 (source-provenance-fixes): `retag_all_handler` must re-index
    /// entity-less chunks into Tantivy, matching `persist_chunk`, instead of
    /// silently skipping them behind the old entity gate.
    ///
    /// Discriminating: with the gate present this asserts `retagged_count == 0`
    /// and an empty Tantivy index — both assertions fail. With the gate removed
    /// the chunk is counted and searchable.
    #[tokio::test]
    async fn retag_all_reindexes_entity_less_chunks() {
        let (_app, state) = crate::tests::make_test_app();

        let chunk = entity_less_chunk(
            "retag-entityless-1",
            "lorem ipsum dolor sit amet",
            "seed-agent",
        );
        state.store.store_chunk(&chunk).unwrap();

        // Pre-condition: nothing indexed yet.
        {
            let tv = state.tantivy.lock().await;
            assert!(
                tv.search("lorem", 10).unwrap().is_empty(),
                "index must start empty"
            );
        }

        let resp = retag_all_handler(State(state.clone()))
            .await
            .expect("retag_all_handler");
        assert_eq!(
            resp.0.retagged_count, 1,
            "entity-less chunk must be reindexed (entity gate removed)"
        );

        let tv = state.tantivy.lock().await;
        let hits = tv.search("lorem", 10).unwrap();
        assert_eq!(
            hits.len(),
            1,
            "entity-less chunk must be searchable after retag_all"
        );
        assert_eq!(hits[0].id, "retag-entityless-1");
    }
}
