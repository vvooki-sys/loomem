//! autoDream — background consolidation worker.
//!
//! Groups chunks by subject, merges observations into consolidated facts,
//! resolves contradictions, and promotes to L1.

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::config::LlmConfig;
use crate::cost_tracker::CostTracker;
use crate::embedding_queue::EmbeddingQueue;
use crate::intent_log::{IntentLog, OpType};
use crate::source_tag::SourceTag;
use crate::storage::{persist_chunk_with_index, Chunk, PersistChunkArgs, RocksDbStore};
use crate::tantivy_index::{TantivyIndex, TextDocument};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DreamConfig {
    pub enabled: bool,
    pub batch_size: usize,
    pub min_group_size: usize,
    pub model: String,
    pub cost_cap_usd_per_run: f64,
    /// Count of newly-persisted chunks (per stream) that arms an automatic dream
    /// run. `0` disables auto-triggering (dream stays purely on-demand). Carried
    /// with `#[serde(default)]` so configs predating this field still parse.
    #[serde(default = "default_auto_trigger_threshold")]
    pub auto_trigger_threshold: usize,
    /// Minimum seconds between two automatic dream runs on the same stream.
    /// Debounces bursty ingests (e.g. bulk imports) so they don't fire runs
    /// back-to-back.
    #[serde(default = "default_auto_cooldown_secs")]
    pub auto_cooldown_secs: u64,
}

/// Default auto-trigger threshold: `0` (disabled), so auto-triggering is strictly
/// opt-in. A config that enables the dream worker (`enabled = true`) but predates
/// this field must NOT silently start firing automatic `dream_run`s (LLM cost) —
/// the user opts in by setting `auto_trigger_threshold` explicitly. The shipped
/// `config.toml` sets `50`, matching `batch_size` (dream truncates its working
/// set to `batch_size`, so a larger value would leave chunks unprocessed).
fn default_auto_trigger_threshold() -> usize {
    0
}

/// Default cooldown between automatic dream runs on the same stream (15 min).
fn default_auto_cooldown_secs() -> u64 {
    900
}

impl Default for DreamConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            batch_size: 50,
            min_group_size: 2,
            model: "gpt-4.1-mini".to_string(),
            cost_cap_usd_per_run: 0.10,
            auto_trigger_threshold: default_auto_trigger_threshold(),
            auto_cooldown_secs: default_auto_cooldown_secs(),
        }
    }
}

const DREAM_PROMPT: &str = r#"You have a group of observations about the same subject: {subject}

Observations (oldest to newest):
{chunks_with_dates}

Task:
1. Extract the most current, true fact about this subject
2. Identify contradictions (e.g. "used X" vs "switched to Y")
3. Return JSON only (no markdown):
{
  "merged_fact": "single sentence — the current truth about this subject",
  "fact_type": "preference_or_decision"|"project_state"|"fact"|"experience",
  "fact_date": "YYYY-MM-DD or null",
  "contradictions": [
    {"old_uuid": "...", "reason": "brief explanation"}
  ],
  "confidence": 0.0-1.0
}

Rules:
- merged_fact must be a single, self-contained sentence
- Only flag genuine contradictions (not refinements or additions)
- If all observations agree, contradictions should be empty
- Preserve names, dates, numbers exactly
- Language: match the observations"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DreamResult {
    pub stream: String,
    pub chunks_processed: usize,
    pub groups_found: usize,
    pub facts_merged: usize,
    pub contradictions_resolved: usize,
    pub cost_usd: f64,
    pub duration_ms: u64,
    pub cost_cap_reached: bool,
}

/// Deserialized dream LLM response. Public so the pure-logic helper
/// `apply_dream_response_for_subject` can be exercised in integration tests
/// without standing up an HTTP mock.
#[derive(Debug, Deserialize)]
pub struct LlmDreamResponse {
    pub merged_fact: String,
    pub fact_type: Option<String>,
    pub fact_date: Option<String>,
    pub contradictions: Vec<LlmContradiction>,
    pub confidence: f64,
}

#[derive(Debug, Deserialize)]
pub struct LlmContradiction {
    pub old_uuid: String,
    pub reason: String,
}

/// Group chunks by subject from extraction_meta.
/// Only includes chunks with is_latest=true and extraction_meta.subject != null.
fn group_by_subject(chunks: Vec<Chunk>) -> HashMap<String, Vec<Chunk>> {
    let mut groups: HashMap<String, Vec<Chunk>> = HashMap::new();

    for chunk in chunks {
        if !chunk.is_latest {
            continue;
        }
        if let Some(ref meta) = chunk.extraction_meta {
            if let Some(ref subject) = meta.subject {
                let key = subject.to_lowercase();
                groups.entry(key).or_default().push(chunk);
            }
        }
    }

    // Sort each group by timestamp (oldest first)
    for chunks in groups.values_mut() {
        chunks.sort_by_key(|c| c.timestamp);
    }

    groups
}

/// Format chunks for the dream consolidation prompt.
fn format_chunks_for_prompt(chunks: &[Chunk]) -> String {
    chunks
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let date = c
                .extraction_meta
                .as_ref()
                .and_then(|m| m.event_date.as_deref())
                .unwrap_or("unknown date");
            format!("{}. [{}] [id:{}] {}", i + 1, date, c.id, c.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Context passed to [`dream_run`] — groups the LLM config, dream config, and optional
/// intent_log handle to keep `dream_run` within the §1 nargs ≤ 6 limit.
///
/// `intent_log` is threaded to `DreamApplyContext` so that
/// `persist_chunk_with_index` receives a `Some` handle on every chunk-persist
/// call, enabling boot recovery for dream-path writes (cycle /47 CS2 fix).
pub struct DreamRunContext<'a> {
    pub llm_config: &'a LlmConfig,
    pub dream_config: &'a DreamConfig,
    pub intent_log: Option<&'a tokio::sync::Mutex<IntentLog>>,
    /// Shared embedding queue. The merged L1 chunk is enqueued here after persist
    /// so it gets a vector via the configured embedder; `None` disables embedding
    /// (e.g. in tests). See `apply_dream_response_for_subject`.
    pub embedding_queue: Option<EmbeddingQueue>,
}

/// Run dream consolidation on a single stream.
pub async fn dream_run(
    store: &RocksDbStore,
    tantivy: &tokio::sync::Mutex<TantivyIndex>,
    client: &Client,
    run_ctx: DreamRunContext<'_>,
    cost_tracker: &CostTracker,
    stream: &str,
) -> Result<DreamResult> {
    let llm_config = run_ctx.llm_config;
    let dream_config = run_ctx.dream_config;
    let intent_log = run_ctx.intent_log;
    let embedding_queue = run_ctx.embedding_queue;

    let start = std::time::Instant::now();
    let mut total_cost = 0.0;
    let mut facts_merged = 0;
    let mut contradictions_resolved = 0;

    // Collect chunks from this stream with extraction_meta
    let mut all_chunks: Vec<Chunk> = Vec::new();
    for level in 0..=1 {
        let prefix = format!("chunk:L{}:", level);
        for (_key, value) in store.prefix_scan(prefix.as_bytes()) {
            if let Ok(chunk) = store.decode_chunk(&value) {
                if chunk.stream == stream
                    && chunk.is_latest
                    && chunk.extraction_meta.is_some()
                    && chunk
                        .extraction_meta
                        .as_ref()
                        .and_then(|m| m.subject.as_ref())
                        .is_some()
                {
                    all_chunks.push(chunk);
                }
            }
        }
    }

    let chunks_processed = all_chunks.len().min(dream_config.batch_size);
    all_chunks.truncate(dream_config.batch_size);

    // Group by subject
    let groups = group_by_subject(all_chunks);
    let groups_found = groups.len();

    info!(
        "Dream run on stream '{}': {} chunks, {} subject groups",
        stream, chunks_processed, groups_found
    );

    let api_key = llm_config
        .get_api_key()
        .context("OpenAI API key not configured for dream worker")?;

    for (subject, chunks) in &groups {
        if chunks.len() < dream_config.min_group_size {
            continue;
        }

        // Cost cap check
        if total_cost >= dream_config.cost_cap_usd_per_run {
            info!(
                "Dream cost cap reached ({:.3} >= {:.3}), stopping",
                total_cost, dream_config.cost_cap_usd_per_run
            );
            return Ok(DreamResult {
                stream: stream.to_string(),
                chunks_processed,
                groups_found,
                facts_merged,
                contradictions_resolved,
                cost_usd: total_cost,
                duration_ms: start.elapsed().as_millis() as u64,
                cost_cap_reached: true,
            });
        }

        // Check idempotency: skip if a dream-consolidated chunk already exists for this subject
        let already_consolidated = store.prefix_scan(b"chunk:L1:").any(|(_k, v)| {
            store.decode_chunk(&v).ok().is_some_and(|c| {
                c.stream == stream
                    && c.source.as_ref().map(|s| s.agent.as_str()) == Some("dream-consolidation")
                    && c.is_latest
                    && c.extraction_meta.as_ref()
                        .and_then(|m| m.subject.as_deref())
                        .map(|s| s.to_lowercase() == *subject)
                        .unwrap_or(false)
                    // Only skip if all source chunks are covered
                    && c.extraction_meta.as_ref()
                        .and_then(|m| m.extracted_from.as_deref())
                        .map(|ids| chunks.iter().all(|ch| ids.contains(&ch.id)))
                        .unwrap_or(false)
            })
        });

        if already_consolidated {
            debug!(
                "Dream: skipping subject '{}' — already consolidated",
                subject
            );
            continue;
        }

        let chunks_text = format_chunks_for_prompt(chunks);
        let prompt = DREAM_PROMPT
            .replace("{subject}", subject)
            .replace("{chunks_with_dates}", &chunks_text);

        // LLM call
        let request_body = serde_json::json!({
            "model": &dream_config.model,
            "messages": [
                {"role": "system", "content": prompt},
                {"role": "user", "content": "Consolidate the observations above."}
            ],
            "max_tokens": 500,
            "temperature": 0.0
        });

        let response = match client
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .json(&request_body)
            .timeout(Duration::from_secs(30))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!("Dream LLM call failed for subject '{}': {}", subject, e);
                // /157 S3: background warn+continue — counted for status.
                crate::llm_failures::global()
                    .record(crate::llm_failures::LlmFailureKind::Consolidation);
                continue;
            }
        };

        if !response.status().is_success() {
            warn!("Dream LLM error for '{}': {}", subject, response.status());
            crate::llm_failures::global()
                .record(crate::llm_failures::LlmFailureKind::Consolidation);
            continue;
        }

        #[derive(Deserialize)]
        struct Resp {
            choices: Vec<Choice>,
            usage: Option<Usage>,
        }
        #[derive(Deserialize)]
        struct Choice {
            message: Msg,
        }
        #[derive(Deserialize)]
        struct Msg {
            content: String,
        }
        #[derive(Deserialize)]
        struct Usage {
            prompt_tokens: u64,
            completion_tokens: u64,
        }

        let resp: Resp = response
            .json()
            .await
            .context("Failed to parse dream LLM response")?;

        // Track cost
        if let Some(ref usage) = resp.usage {
            let cost = (usage.prompt_tokens as f64 * 0.15 + usage.completion_tokens as f64 * 0.60)
                / 1_000_000.0;
            total_cost += cost;
            let _ = cost_tracker.record(
                usage.prompt_tokens,
                usage.completion_tokens,
                &dream_config.model,
            );
        }

        let content = resp
            .choices
            .first()
            .map(|c| c.message.content.clone())
            .unwrap_or_default();
        let json_str = content
            .trim()
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();

        let dream_resp: LlmDreamResponse = match serde_json::from_str(json_str) {
            Ok(r) => r,
            Err(e) => {
                warn!("Dream: failed to parse response for '{}': {}", subject, e);
                continue;
            }
        };

        let (merged_delta, blocked_delta) = apply_dream_response_for_subject(
            store,
            tantivy,
            DreamApplyContext {
                stream,
                subject,
                chunks,
                model: &dream_config.model,
                intent_log,
                embedding_queue: embedding_queue.clone(),
            },
            dream_resp,
        )
        .await?;
        facts_merged += merged_delta;
        contradictions_resolved += blocked_delta;
    }

    Ok(DreamResult {
        stream: stream.to_string(),
        chunks_processed,
        groups_found,
        facts_merged,
        contradictions_resolved,
        cost_usd: total_cost,
        duration_ms: start.elapsed().as_millis() as u64,
        cost_cap_reached: false,
    })
}

/// Context passed to `apply_dream_response_for_subject` — groups the
/// per-subject processing parameters so the function stays within the §1
/// nargs ≤ 6 limit (cycle /46 fixup: original had nargs=7).
///
/// `intent_log` added in cycle /47 (CS2 fix): when `Some`, the persist helper
/// registers a pending intent-log entry so boot recovery can replay the Tantivy
/// write if the server crashes between the RocksDB and Tantivy writes.
pub struct DreamApplyContext<'a> {
    pub stream: &'a str,
    pub subject: &'a str,
    pub chunks: &'a [Chunk],
    pub model: &'a str,
    pub intent_log: Option<&'a tokio::sync::Mutex<IntentLog>>,
    /// Shared embedding queue for the merged chunk (see `DreamRunContext`).
    pub embedding_queue: Option<EmbeddingQueue>,
}

/// Walk `dream_resp.contradictions` through the trust guard for one subject.
///
/// Extracted from `apply_dream_response_for_subject` to reduce orchestrator
/// NLOC and CC (cycle /46 fixup).
fn resolve_dream_contradictions(
    store: &RocksDbStore,
    stream: &str,
    new_id: &str,
    contradictions: &[LlmContradiction],
) -> Result<usize> {
    let mut resolved = 0;
    for contradiction in contradictions {
        let Ok(Some(old_chunk)) = store.get_chunk(&contradiction.old_uuid) else {
            continue;
        };
        if !(old_chunk.is_latest && old_chunk.stream == stream) {
            continue;
        }
        // Cycle /40a: apply valid_until + superseded_by in the same
        // store_chunk call via extra_old_mutator to close the R-M-W window.
        let applied = crate::contradiction::try_supersede_with_guard(
            store,
            &old_chunk,
            new_id,
            Some("a2"),
            "dream",
            Some(&|c: &mut Chunk| {
                c.valid_until = Some(now_secs());
                if let Some(ref mut meta) = c.extraction_meta {
                    meta.superseded_by = Some("dream-consolidated".to_string());
                }
            }),
        )?;
        if applied {
            resolved += 1;
            debug!(
                "Dream: superseded chunk {} ({})",
                contradiction.old_uuid, contradiction.reason
            );
        } else {
            debug!(
                "Dream: supersede blocked by trust guard for {} (reason: {})",
                contradiction.old_uuid, contradiction.reason
            );
        }
    }
    Ok(resolved)
}

/// Build the new `Chunk` for a dream-consolidated merged fact.
///
/// Extracted from `apply_dream_response_for_subject` to reduce orchestrator
/// NLOC (cycle /46 fixup). The Chunk struct literal is large; isolating it
/// here keeps the orchestrator readable.
fn build_dream_chunk(
    new_id: &str,
    ctx: &DreamApplyContext<'_>,
    dream_resp: &LlmDreamResponse,
    source_ids: Vec<String>,
    fact_type: crate::storage::FactType,
) -> Chunk {
    Chunk {
        id: new_id.to_string(),
        content: dream_resp.merged_fact.clone(),
        stream: ctx.stream.to_string(),
        level: 1,
        score: 1.0,
        timestamp: now_secs(),
        consolidated: false,
        dormant: false,
        in_progress: false,
        prompt_version: None,
        source_ids: Some(source_ids.clone()),
        last_decay: None,
        metadata: None,
        importance: Some(dream_resp.confidence),
        persistent: true,
        last_implicit_boost: None,
        access_count: 0,
        source: Some(SourceTag::from_agent("dream-consolidation")),
        created_by: Some("loomem-dream".to_string()),
        updated_at: Some(now_secs()),
        valid_from: dream_resp.fact_date.as_ref().and_then(|d| parse_date(d)),
        valid_until: None,
        is_latest: true,
        superseded_by: None,
        supersedes_id: None,
        root_memory_id: None,
        version: 1,
        memory_type: Some("static".to_string()),
        extraction_meta: Some(crate::storage::ExtractionMeta {
            fact_type,
            subject: Some(ctx.subject.to_string()),
            event_date: dream_resp.fact_date.clone(),
            event_date_context: None,
            supersedes: None,
            superseded_by: None,
            confidence: dream_resp.confidence,
            extracted_from: Some(source_ids.join(",")),
            extraction_model: Some(ctx.model.to_string()),
            original_content: None,
            topic: None,
            // attributed_to tracks the source statement's speaker; this
            // synthesized chunk has no single source statement, so leave it None.
            attributed_to: None,
        }),
        deleted_at: None,
        // Cycle /40: dream output is assistant_generated -> A2 (derived
        // trust). The trust guard in try_supersede_with_guard relies on
        // this being explicit so blocks are precise (no None->"a1" fallback).
        trust_level: Some("a2".to_string()),
        ingester_user_id: None,
        alpha: 1.0,
        beta: 1.0,
        harmful_count: 0,
        n_ratings: 0,
        last_rated_at: None,
        provenance_role: crate::storage::ProvenanceRole::Claim,
    }
}

/// Build the `TextDocument` for a dream-consolidated chunk.
///
/// Extracted from `apply_dream_response_for_subject` to reduce NLOC of the
/// orchestrator (cycle /46 fixup: original NLOC=139, limit ≤100).
fn build_dream_text_doc(chunk: &Chunk) -> TextDocument {
    // Parse event_date from extraction_meta for Tantivy temporal indexing.
    // and_hms_opt(12,0,0) returns None only for invalid h/m/s values;
    // 12:00:00 is always valid, so the and_then chain returns None only
    // when the date string is absent or unparseable.
    let event_date: Option<i64> = chunk
        .extraction_meta
        .as_ref()
        .and_then(|m| m.event_date.as_ref())
        .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
        .and_then(|d| d.and_hms_opt(12, 0, 0))
        .map(|dt| dt.and_utc().timestamp());

    TextDocument {
        id: chunk.id.clone(),
        content: chunk.content.clone(),
        // Dream chunks are system-generated; no user/app context.
        user_id: "dream".to_string(),
        app_id: "dream".to_string(),
        level: chunk.level,
        timestamp: chunk.timestamp as i64,
        stream: chunk.stream.clone(),
        entities: None,
        relations: None,
        event_date,
        source_agent: chunk.source.as_ref().map(|s| s.agent.clone()),
    }
}

/// Pure-logic apply step for a single subject's dream LLM response: walks the
/// LLM-flagged contradictions through the trust guard and writes the new
/// merged-fact chunk. Returned tuple is `(facts_merged_delta,
/// contradictions_resolved_delta)`.
///
/// Extracted from `dream_run` so the storage-mutating portion of the pipeline
/// can be exercised end-to-end in integration tests without standing up an
/// HTTP mock for the LLM call (cycle /40a). The trust-guard semantics here
/// are byte-identical to the inline body that lived in `dream_run` before
/// the extraction; only the call site changed.
/// Intentional architectural change (cycle /46): dream now writes to Tantivy
/// via `persist_chunk_with_index`. Before /46 this function was sync and
/// only persisted to RocksDB (cycle/38 H4 finding #1: dream output missing
/// from Tantivy index). Making it async is required by the async helper.
///
/// Cycle /46 fixup: `stream`, `subject`, `chunks`, `model` packed into
/// `DreamApplyContext` to satisfy §1 nargs ≤ 6. `build_dream_text_doc`
/// extracted to reduce NLOC and improve MI.
pub async fn apply_dream_response_for_subject(
    store: &RocksDbStore,
    tantivy: &tokio::sync::Mutex<TantivyIndex>,
    ctx: DreamApplyContext<'_>,
    dream_resp: LlmDreamResponse,
) -> Result<(usize, usize)> {
    // Pre-generate the new chunk id so we can pass it to the trust guard
    // BEFORE flipping any old chunk's is_latest. (Cycle /40)
    let new_id = format!("dream:{}", uuid::Uuid::new_v4());

    // Resolve contradictions through the trust guard. Extracted to helper
    // to keep orchestrator within §1 NLOC/CC limits (cycle /46 fixup).
    let contradictions_resolved =
        resolve_dream_contradictions(store, ctx.stream, &new_id, &dream_resp.contradictions)?;

    // Build and persist merged fact as new L1 chunk.
    let source_ids: Vec<String> = ctx.chunks.iter().map(|c| c.id.clone()).collect();
    let fact_type = match dream_resp.fact_type.as_deref() {
        Some("preference_or_decision") => crate::storage::FactType::PreferenceOrDecision,
        Some("project_state") => crate::storage::FactType::ProjectState,
        Some("experience") => crate::storage::FactType::Experience,
        _ => crate::storage::FactType::Fact,
    };
    let new_chunk = build_dream_chunk(&new_id, &ctx, &dream_resp, source_ids, fact_type);
    let text_doc = build_dream_text_doc(&new_chunk);
    // L1 chunk from dream consolidation pipeline.
    // OpType::Consolidate is semantically accurate; recover() is symmetric
    // for Store and Consolidate since /51 PR #106.
    persist_chunk_with_index(
        store,
        tantivy,
        PersistChunkArgs {
            chunk: &new_chunk,
            text_doc,
            intent_log: ctx.intent_log,
            op: OpType::Consolidate,
        },
    )
    .await?;

    // Enqueue the merged L1 chunk for embedding via the shared queue (which uses
    // the configured local/OpenAI embedder). Without this, dream-consolidated
    // chunks persist to RocksDB + Tantivy but never receive a vector, so they
    // stay permanently "pending" in memory_status and never participate in
    // vector retrieval (BM25 only). warn-skip on a full/closed queue, mirroring
    // the ingest path.
    if let Some(ref queue) = ctx.embedding_queue {
        if let Err(e) = queue.enqueue(new_id.clone(), new_chunk.content.clone()) {
            warn!(
                "Failed to enqueue dream chunk {} for embedding: {}",
                new_id, e
            );
        }
    }

    info!(
        "Dream: merged {} chunks for '{}' → {}",
        ctx.chunks.len(),
        ctx.subject,
        new_id
    );

    Ok((1, contradictions_resolved))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn parse_date(date_str: &str) -> Option<u64> {
    chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc().timestamp() as u64)
}
