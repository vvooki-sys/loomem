//! Contradiction detection and memory versioning.
//!
//! Two-step algorithm inspired by Supermemory:
//! 1. Fast vector screen: cosine similarity > threshold
//! 2. LLM classification: updates (contradiction), extends (enrichment), or none

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{debug, warn};

use crate::config::LlmConfig;
use crate::storage::{Chunk, RocksDbStore};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContradictionConfig {
    pub enabled: bool,
    pub similarity_threshold: f64,
    pub max_candidates: usize,
    pub model: String,
    /// /156: when true, a trust-OK supersede merges old+new into one
    /// trajectory-carrying sentence via an LLM rewrite. Default false keeps
    /// the supersede path byte-identical (serde default keeps a config
    /// without this field deserializing unchanged).
    #[serde(default)]
    pub history_preserving_rewrite: bool,
}

impl Default for ContradictionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            similarity_threshold: 0.70,
            max_candidates: 5,
            model: "gpt-4.1-mini".to_string(),
            history_preserving_rewrite: false,
        }
    }
}

const CLASSIFY_PROMPT: &str = r#"You are a memory contradiction detector. Given an OLD memory and a NEW memory from the same person, classify the relationship:

- UPDATES: The new memory contradicts or replaces the old one (e.g., changed preference, corrected fact, new status)
- EXTENDS: The new memory adds detail to the old one without contradicting it (e.g., more specific information, additional context)
- NONE: The memories are unrelated despite surface similarity

Return ONLY valid JSON (no markdown, no code blocks):
{"relation": "updates"|"extends"|"none", "reason": "brief explanation"}"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassificationResult {
    pub relation: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct ContradictionCandidate {
    pub chunk: Chunk,
    pub similarity: f64,
}

/// Find similar chunks in the same stream using vector similarity.
pub fn find_candidates(
    store: &RocksDbStore,
    new_embedding: &[f32],
    stream: &str,
    config: &ContradictionConfig,
) -> Result<Vec<ContradictionCandidate>> {
    let all_embeddings = store.get_all_embeddings()?;
    let mut candidates = Vec::new();

    for (chunk_id, existing_emb) in &all_embeddings {
        let sim = cosine_similarity(new_embedding, existing_emb);
        if sim < config.similarity_threshold {
            continue;
        }

        // Load chunk to check stream match, is_latest, and tombstone status.
        // cycle/80: defense-in-depth filter for tombstoned chunks. /78 fixed
        // delete_memory_fully to hard-delete embeddings, but legacy zombies
        // pre-dating that fix can still sit in CF_EMBEDDINGS until the
        // ~30-day hard-purge window. Without this filter, a tombstoned
        // chunk with high cosine similarity would be returned as a
        // candidate and downstream classify_relation would burn LLM calls
        // on dead content.
        if let Some(chunk) = store.get_chunk(chunk_id)? {
            if chunk.stream == stream && chunk.is_latest && chunk.deleted_at.is_none() {
                candidates.push(ContradictionCandidate {
                    chunk,
                    similarity: sim,
                });
            }
        }
    }

    // Sort by similarity descending, take top N
    candidates.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.truncate(config.max_candidates);

    debug!(
        "Contradiction screen: {} candidates above threshold {} in stream {}",
        candidates.len(),
        config.similarity_threshold,
        stream
    );

    Ok(candidates)
}

/// Classify relationship between old and new memory using LLM.
pub async fn classify_relation(
    client: &Client,
    llm_config: &LlmConfig,
    model: &str,
    old_content: &str,
    new_content: &str,
) -> Result<ClassificationResult> {
    let api_key = llm_config
        .get_api_key()
        .context("OpenAI API key not configured for contradiction detection")?;

    let user_message = format!("OLD memory: {}\nNEW memory: {}", old_content, new_content);

    let request_body = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": CLASSIFY_PROMPT},
            {"role": "user", "content": user_message}
        ],
        "max_tokens": 100,
        "temperature": 0.0
    });

    let response = client
        .post("https://api.openai.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&request_body)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("Failed to send contradiction classification request")?;

    let status = response.status();
    if !status.is_success() {
        let error_text = response.text().await.unwrap_or_default();
        anyhow::bail!("Contradiction LLM call failed ({}): {}", status, error_text);
    }

    #[derive(Deserialize)]
    struct LlmResponse {
        choices: Vec<LlmChoice>,
    }
    #[derive(Deserialize)]
    struct LlmChoice {
        message: LlmMessage,
    }
    #[derive(Deserialize)]
    struct LlmMessage {
        content: String,
    }

    let llm_resp: LlmResponse = response
        .json()
        .await
        .context("Failed to parse contradiction LLM response")?;

    let content = llm_resp
        .choices
        .first()
        .map(|c| c.message.content.clone())
        .unwrap_or_default();

    // Parse JSON from LLM response (handle potential markdown wrapping)
    let json_str = content
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    match serde_json::from_str::<ClassificationResult>(json_str) {
        Ok(result) => {
            debug!(
                "Contradiction classified: {} (reason: {})",
                result.relation, result.reason
            );
            Ok(result)
        }
        Err(e) => {
            warn!(
                "Failed to parse contradiction LLM response '{}': {}",
                json_str, e
            );
            // Default to NONE on parse failure (safe fallback)
            Ok(ClassificationResult {
                relation: "none".to_string(),
                reason: format!("Parse failure, defaulting to none: {}", e),
            })
        }
    }
}

/// /156: system prompt for the history-preserving rewrite. The rewrite merges
/// an OLD and NEW memory about the same subject into a single sentence that
/// carries the trajectory of change (where things were → where they are now).
const REWRITE_PROMPT: &str = r#"You merge an OLD memory and a NEW memory about the same subject into ONE sentence that preserves the trajectory of change — where things were, and where they are now.

Rules:
- Write in the 3rd person, declarative voice.
- Output exactly one self-contained sentence carrying both the prior and the current state.
- No meta-words about memory or the update process. No provenance. Do NOT write phrases like "previously stated" or "according to memory".
- Preserve the original language and diacritics.

Examples:
- OLD: "Anna is an ML engineer at Acme." NEW: "Anna is the CEO of Acme." -> "Anna, formerly an ML engineer at Acme, is now its CEO."
- OLD: "Marek uses VS Code as his editor." NEW: "Marek uses Cursor as his editor." -> "Marek switched his editor from VS Code to Cursor."

Return ONLY the merged sentence — no markdown, no quotes, no JSON."#;

/// Build the user message for the history-preserving rewrite: the OLD and NEW
/// memories with their event dates (or "unknown date" when absent).
fn build_rewrite_user_message(
    old_content: &str,
    new_content: &str,
    old_date: Option<&str>,
    new_date: Option<&str>,
) -> String {
    format!(
        "OLD memory (from {}): {}\nNEW memory (from {}): {}",
        old_date.unwrap_or("unknown date"),
        old_content,
        new_date.unwrap_or("unknown date"),
        new_content,
    )
}

/// /156: merge old+new into one trajectory-carrying sentence via the LLM.
///
/// Returns the rewritten sentence on success. Errors (no API key, non-2xx,
/// transport/parse failure, or an empty completion) are surfaced to the caller
/// via `Err` so it can fall back to the original content without blocking the
/// write. Mirrors [`classify_relation`]'s HTTP shape.
pub async fn rewrite_with_history(
    client: &Client,
    llm_config: &LlmConfig,
    model: &str,
    old_content: &str,
    new_content: &str,
    old_date: Option<&str>,
    new_date: Option<&str>,
) -> Result<String> {
    let api_key = llm_config
        .get_api_key()
        .context("OpenAI API key not configured for history-preserving rewrite")?;

    let user_message = build_rewrite_user_message(old_content, new_content, old_date, new_date);

    let request_body = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": REWRITE_PROMPT},
            {"role": "user", "content": user_message}
        ],
        "max_tokens": 200,
        "temperature": 0.0
    });

    let response = client
        .post("https://api.openai.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&request_body)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("Failed to send history-preserving rewrite request")?;

    let status = response.status();
    if !status.is_success() {
        let error_text = response.text().await.unwrap_or_default();
        anyhow::bail!("Rewrite LLM call failed ({}): {}", status, error_text);
    }

    #[derive(Deserialize)]
    struct LlmResponse {
        choices: Vec<LlmChoice>,
    }
    #[derive(Deserialize)]
    struct LlmChoice {
        message: LlmMessage,
    }
    #[derive(Deserialize)]
    struct LlmMessage {
        content: String,
    }

    let llm_resp: LlmResponse = response
        .json()
        .await
        .context("Failed to parse history-preserving rewrite response")?;

    let content = llm_resp
        .choices
        .first()
        .map(|c| c.message.content.clone())
        .unwrap_or_default();

    let rewritten = content.trim().trim_matches('"').trim().to_string();

    if rewritten.is_empty() {
        anyhow::bail!("History-preserving rewrite returned empty content");
    }

    Ok(rewritten)
}

/// Trust rank for the supersede guard: a1=3, a2=2, b=1, unknown=0.
///
/// `None` is treated as `"a1"` for backward compatibility with legacy chunks
/// written before trust tiers existed (see `derive_trust_level` in storage).
pub fn trust_rank(t: Option<&str>) -> u8 {
    match t.unwrap_or("a1") {
        "a1" => 3,
        "a2" => 2,
        "b" => 1,
        _ => 0,
    }
}

/// Append a `trust_guard_blocked` audit entry. Best-effort: failure to append
/// the audit log does not block the caller's normal path (mirrors B2 audit
/// semantics — admin actions commit even if audit append fails).
fn append_trust_guard_audit(
    store: &RocksDbStore,
    old_chunk: &Chunk,
    new_chunk_id: &str,
    new_trust: Option<&str>,
    context: &str,
) {
    let details = serde_json::json!({
        "op": "trust_guard_blocked",
        "old_chunk_id": old_chunk.id,
        "old_trust": old_chunk.trust_level.as_deref().unwrap_or("a1"),
        "new_chunk_id": new_chunk_id,
        "new_trust": new_trust.unwrap_or("a1"),
        "context": context,
    });
    let event = crate::audit::AuditEvent::system("trust_guard_blocked", details);
    if let Err(e) = crate::audit::append(store, &old_chunk.stream, &event) {
        warn!(
            "trust guard ({context}): failed to append audit entry for old={} new={}: {e}",
            old_chunk.id, new_chunk_id
        );
    }
}

/// Apply supersede: mark old chunk as superseded, link new chunk.
/// Returns the updated new_chunk with version chain fields set.
/// Trust hierarchy enforced: lower-trust content cannot supersede higher-trust.
/// On guard violation, an audit entry with `action: "trust_guard_blocked"` and
/// `context: "contradiction"` is appended for `old_chunk.stream`.
pub fn apply_supersede(store: &RocksDbStore, old_chunk: &Chunk, new_chunk: Chunk) -> Result<Chunk> {
    // Trust hierarchy check: B cannot supersede A1/A2, A2 cannot supersede A1.
    let old_rank = trust_rank(old_chunk.trust_level.as_deref());
    let new_rank = trust_rank(new_chunk.trust_level.as_deref());

    if new_rank < old_rank {
        append_trust_guard_audit(
            store,
            old_chunk,
            &new_chunk.id,
            new_chunk.trust_level.as_deref(),
            "contradiction",
        );
        tracing::info!(
            "Trust guard (contradiction): {} (trust={}) cannot supersede {} (trust={}), storing as separate",
            new_chunk.id,
            new_chunk.trust_level.as_deref().unwrap_or("a1"),
            old_chunk.id,
            old_chunk.trust_level.as_deref().unwrap_or("a1"),
        );
        // Don't supersede — just return the new chunk as-is (stored separately).
        return Ok(new_chunk);
    }

    // Update old chunk
    let mut updated_old = old_chunk.clone();
    updated_old.is_latest = false;
    updated_old.superseded_by = Some(new_chunk.id.clone());
    updated_old.updated_at = Some(now_unix_secs());
    store.store_chunk(&updated_old)?;

    // Update new chunk with version chain
    let new_chunk = link_successor(old_chunk, new_chunk);

    debug!(
        "Superseded chunk {} (v{}) → {} (v{})",
        old_chunk.id, old_chunk.version, new_chunk.id, new_chunk.version
    );

    Ok(new_chunk)
}

/// Set the version-chain fields on `new_chunk` so it becomes the successor of
/// `old_chunk` (`supersedes_id`, `root_memory_id`, `version`). Pure — writes
/// nothing. `apply_supersede` uses it after flipping the old chunk; callers
/// that must persist the successor *before* flipping the old chunk (so a
/// failed write cannot strand the old one as non-latest) use it directly and
/// flip with `try_supersede_with_guard` afterwards.
pub fn link_successor(old_chunk: &Chunk, mut new_chunk: Chunk) -> Chunk {
    new_chunk.supersedes_id = Some(old_chunk.id.clone());
    new_chunk.root_memory_id = Some(
        old_chunk
            .root_memory_id
            .clone()
            .unwrap_or_else(|| old_chunk.id.clone()),
    );
    new_chunk.version = old_chunk.version + 1;
    new_chunk
}

/// Try to mark `old_chunk` as superseded by `new_chunk_id` (with trust level
/// `new_trust`), enforcing the trust hierarchy guard.
///
/// Returns `Ok(true)` if the supersede was applied (`old_chunk` written with
/// `is_latest=false`, `superseded_by=new_chunk_id`). Returns `Ok(false)` if
/// blocked by the trust guard (old chunk not modified). On block, a
/// `trust_guard_blocked` audit entry is appended for `old_chunk.stream` with
/// the supplied `context` ("dream" / "consolidation" / etc.).
///
/// Used by callers that build their own new chunk separately and only need to
/// flip `is_latest` on the old one — i.e. no version-chain reconstruction
/// (which is what `apply_supersede` does).
///
/// `extra_old_mutator` runs on the cloned old chunk **after** the helper sets
/// `is_latest=false`/`superseded_by`/`updated_at` and **before** the single
/// `store_chunk` call. This lets callers layer caller-specific bookkeeping
/// (e.g. dream's `valid_until` and `extraction_meta.superseded_by`) into the
/// same write so a reader cannot observe `is_latest=false` while the
/// caller-specific fields are still stale. The mutator only runs on the
/// apply path; on guard block it is **not** invoked.
pub fn try_supersede_with_guard(
    store: &RocksDbStore,
    old_chunk: &Chunk,
    new_chunk_id: &str,
    new_trust: Option<&str>,
    context: &str,
    extra_old_mutator: Option<&dyn Fn(&mut Chunk)>,
) -> Result<bool> {
    let old_rank = trust_rank(old_chunk.trust_level.as_deref());
    let new_rank = trust_rank(new_trust);

    if new_rank < old_rank {
        append_trust_guard_audit(store, old_chunk, new_chunk_id, new_trust, context);
        tracing::info!(
            "Trust guard ({}): {} (trust={}) cannot supersede {} (trust={}), audit logged",
            context,
            new_chunk_id,
            new_trust.unwrap_or("a1"),
            old_chunk.id,
            old_chunk.trust_level.as_deref().unwrap_or("a1"),
        );
        return Ok(false);
    }

    let mut updated_old = old_chunk.clone();
    updated_old.is_latest = false;
    updated_old.superseded_by = Some(new_chunk_id.to_string());
    updated_old.updated_at = Some(now_unix_secs());
    if let Some(f) = extra_old_mutator {
        f(&mut updated_old);
    }
    store
        .store_chunk(&updated_old)
        .context("trust guard: persist superseded old_chunk")?;
    Ok(true)
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Apply extend: link new chunk to root without superseding.
/// Returns the updated new_chunk with root_memory_id set.
pub fn apply_extend(old_chunk: &Chunk, mut new_chunk: Chunk) -> Chunk {
    new_chunk.root_memory_id = Some(
        old_chunk
            .root_memory_id
            .clone()
            .unwrap_or_else(|| old_chunk.id.clone()),
    );

    debug!(
        "Extended chain: {} linked to root {}",
        new_chunk.id,
        new_chunk.root_memory_id.as_deref().unwrap_or("?")
    );

    new_chunk
}

/// Get the version chain for a chunk (root → v1 → v2 → … → latest), restricted
/// to the branch that contains `chunk_id`.
///
/// Walks `supersedes_id` backwards from `chunk_id` to the root, then
/// `superseded_by` forwards from `chunk_id`. Two contracts (brief 2026-09-02,
/// B1 + B2):
///
/// - Soft-deleted versions (`deleted_at` set) are traversed for linkage but
///   never returned. A chunk a human deleted must not resurface through
///   history — `memory_delete` promises removal from every read path.
/// - The result always contains `chunk_id` when it exists and is not deleted.
///   The previous root-first walk followed the root's `superseded_by` pointer,
///   so when one chunk had two successors (a branch) the walk picked the last
///   writer and the queried id vanished from its own history.
///
/// `limit` bounds the walk itself, not just the output: at most `limit`
/// versions are returned and the walk stops as soon as they are collected
/// (older ones first, then newer), so a long chain never costs more than a
/// handful of reads beyond `limit`.
pub fn get_memory_chain(store: &RocksDbStore, chunk_id: &str, limit: usize) -> Result<Vec<Chunk>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let Some(queried) = store.get_chunk(chunk_id)? else {
        return Ok(Vec::new());
    };
    let mut visited = std::collections::HashSet::from([queried.id.clone()]);
    let mut backward_id = queried.supersedes_id.clone();
    let mut forward_id = queried.superseded_by.clone();
    let mut chain: Vec<Chunk> = Vec::new();
    if queried.deleted_at.is_none() {
        chain.push(queried);
    }

    // Backwards: chunk_id → root via `supersedes_id` (collected newest-first).
    while let Some(id) = backward_id.take() {
        if chain.len() >= limit || !visited.insert(id.clone()) {
            break; // limit reached, or cycle
        }
        let Some(chunk) = store.get_chunk(&id)? else {
            break;
        };
        backward_id = chunk.supersedes_id.clone();
        if chunk.deleted_at.is_none() {
            chain.push(chunk);
        }
    }
    chain.reverse();

    // Forwards: from the queried chunk's successor via `superseded_by`.
    while let Some(id) = forward_id.take() {
        if chain.len() >= limit || !visited.insert(id.clone()) {
            break; // limit reached, or cycle
        }
        let Some(chunk) = store.get_chunk(&id)? else {
            break;
        };
        forward_id = chunk.superseded_by.clone();
        if chunk.deleted_at.is_none() {
            chain.push(chunk);
        }
    }

    Ok(chain)
}

/// Result of dedup check.
#[derive(Debug)]
pub enum DedupResult {
    /// New fact — no duplicate found, should store.
    New,
    /// Duplicate found — skip storing, existing chunk was bumped.
    Duplicate(String),
}

/// Check if a new fact is a duplicate of an existing chunk.
///
/// Dedup criteria: cosine similarity > threshold AND same subject.
/// On match: bump access_count + updated_at on existing chunk (UPSERT behavior).
pub fn dedup_check(
    store: &RocksDbStore,
    new_embedding: &[f32],
    stream: &str,
    subject: Option<&str>,
    threshold: f64,
) -> Result<DedupResult> {
    let all_embeddings = store.get_all_embeddings()?;
    let mut best: Option<(String, f64)> = None;

    for (chunk_id, existing_emb) in &all_embeddings {
        let sim = cosine_similarity(new_embedding, existing_emb);
        if sim >= threshold {
            if let Some(chunk) = store.get_chunk(chunk_id)? {
                // cycle/80: skip tombstoned chunks. Without this guard, a
                // soft-deleted chunk with similar content would match here
                // and the caller would treat new ingest as Duplicate(id),
                // bumping the tombstone's access_count instead of storing
                // the new chunk — silent write loss masked as dedup hit.
                if chunk.stream != stream || !chunk.is_latest || chunk.deleted_at.is_some() {
                    continue;
                }
                // Check subject match
                let subject_match = match (
                    subject,
                    chunk
                        .extraction_meta
                        .as_ref()
                        .and_then(|m| m.subject.as_deref()),
                ) {
                    (Some(new_s), Some(old_s)) => new_s.to_lowercase() == old_s.to_lowercase(),
                    (None, None) => true, // both have no subject — match on cosine alone
                    _ => false,
                };
                if subject_match && best.as_ref().is_none_or(|(_, s)| sim > *s) {
                    best = Some((chunk_id.clone(), sim));
                }
            }
        }
    }

    if let Some((chunk_id, _)) = best {
        // UPSERT: bump access_count + updated_at on existing
        if let Some(mut existing) = store.get_chunk(&chunk_id)? {
            existing.access_count += 1;
            existing.updated_at = Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            );
            store.store_chunk(&existing)?;
            debug!(
                "Dedup hit: bumped chunk {} (access_count={})",
                chunk_id, existing.access_count
            );
        }
        Ok(DedupResult::Duplicate(chunk_id))
    } else {
        Ok(DedupResult::New)
    }
}

/// Result of contradiction detection.
#[derive(Debug)]
pub enum ContradictionResult {
    /// No contradiction — store as new.
    None,
    /// Contradiction found — old chunk was superseded.
    Contradiction { old_chunk_id: String },
    /// Refinement — new fact extends old, link but don't supersede.
    Refinement { old_chunk_id: String },
}

/// Detect contradiction against existing chunks.
///
/// Only runs for PreferenceOrDecision and ProjectState types (Fact type is skipped).
/// Early exit if top candidate cosine < cosine_min (no close enough candidate).
pub async fn detect_contradiction(
    client: &Client,
    llm_config: &LlmConfig,
    store: &RocksDbStore,
    new_embedding: &[f32],
    new_content: &str,
    new_fact_type: &str,
    stream: &str,
    model: &str,
    cosine_min: f64,
    subject: Option<&str>,
) -> Result<ContradictionResult> {
    // Skip contradiction check for biographical facts (they rarely change)
    if new_fact_type == "fact" {
        return Ok(ContradictionResult::None);
    }

    // Find top-3 candidates filtered by same subject
    let all_embeddings = store.get_all_embeddings()?;
    let mut candidates: Vec<ContradictionCandidate> = Vec::new();

    for (chunk_id, existing_emb) in &all_embeddings {
        let sim = cosine_similarity(new_embedding, existing_emb);
        if sim < cosine_min {
            continue;
        }
        if let Some(chunk) = store.get_chunk(chunk_id)? {
            // cycle/80: skip tombstoned chunks. Without this guard, the LLM
            // contradiction classifier would be invoked on dead content
            // (LLM cost waste) and a "refinement" classification would
            // attach superseded_by to a tombstone — broken version chain.
            if chunk.stream != stream || !chunk.is_latest || chunk.deleted_at.is_some() {
                continue;
            }
            // Filter by same subject if provided
            let subject_match = match (
                subject,
                chunk
                    .extraction_meta
                    .as_ref()
                    .and_then(|m| m.subject.as_deref()),
            ) {
                (Some(new_s), Some(old_s)) => new_s.to_lowercase() == old_s.to_lowercase(),
                _ => true, // if either has no subject, don't filter
            };
            if subject_match {
                candidates.push(ContradictionCandidate {
                    chunk,
                    similarity: sim,
                });
            }
        }
    }

    candidates.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.truncate(3);

    if candidates.is_empty() {
        return Ok(ContradictionResult::None);
    }

    // LLM judge on best candidate
    let top = &candidates[0];
    match classify_relation(client, llm_config, model, &top.chunk.content, new_content).await {
        Ok(class) => match class.relation.as_str() {
            "updates" => Ok(ContradictionResult::Contradiction {
                old_chunk_id: top.chunk.id.clone(),
            }),
            "extends" => Ok(ContradictionResult::Refinement {
                old_chunk_id: top.chunk.id.clone(),
            }),
            _ => Ok(ContradictionResult::None),
        },
        Err(e) => {
            warn!("Contradiction classification failed: {}", e);
            Ok(ContradictionResult::None) // safe fallback
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// /156: the rewrite prompt demands 3rd-person, declarative output.
    #[test]
    fn rewrite_prompt_is_third_person_declarative() {
        assert!(REWRITE_PROMPT.contains("3rd person"));
        assert!(REWRITE_PROMPT.contains("declarative"));
    }

    /// /156: the prompt forbids meta-words and provenance.
    #[test]
    fn rewrite_prompt_forbids_meta_and_provenance() {
        assert!(REWRITE_PROMPT.contains("No meta-words"));
        assert!(REWRITE_PROMPT.contains("No provenance"));
    }

    /// /156: the forbidden phrases appear as negative examples.
    #[test]
    fn rewrite_prompt_lists_forbidden_phrases_as_negatives() {
        assert!(REWRITE_PROMPT.contains("previously stated"));
        assert!(REWRITE_PROMPT.contains("according to memory"));
    }

    /// /156: the few-shot covers a job change (ML engineer -> CEO).
    #[test]
    fn rewrite_prompt_fewshot_covers_job_change() {
        assert!(REWRITE_PROMPT.contains("ML engineer"));
        assert!(REWRITE_PROMPT.contains("CEO"));
    }

    /// /156: the few-shot covers a tool-preference change (VS Code -> Cursor).
    #[test]
    fn rewrite_prompt_fewshot_covers_tool_pref_change() {
        assert!(REWRITE_PROMPT.contains("VS Code"));
        assert!(REWRITE_PROMPT.contains("Cursor"));
    }

    /// /156: the user message carries both states and both dates.
    #[test]
    fn build_rewrite_user_message_includes_both_states_and_dates() {
        let msg = build_rewrite_user_message(
            "Anna is an ML engineer.",
            "Anna is the CEO.",
            Some("2025-01-01"),
            Some("2026-06-01"),
        );
        assert!(msg.contains("Anna is an ML engineer."));
        assert!(msg.contains("Anna is the CEO."));
        assert!(msg.contains("2025-01-01"));
        assert!(msg.contains("2026-06-01"));
        // absent dates fall back to a placeholder.
        let msg2 = build_rewrite_user_message("a", "b", None, None);
        assert!(msg2.contains("unknown date"));
    }

    // ── get_memory_chain: soft-delete + branch contracts (brief 2026-09-02) ──

    fn chain_store() -> (tempfile::TempDir, RocksDbStore) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let cfg = crate::config::RocksDbConfig {
            max_open_files: 100,
            compression: "lz4".to_string(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        };
        let store = RocksDbStore::open(tmp.path(), &cfg).expect("open store");
        (tmp, store)
    }

    fn chain_chunk(id: &str, version: u32) -> Chunk {
        Chunk {
            id: id.to_string(),
            content: format!("content for {id}"),
            stream: "s".to_string(),
            level: 0,
            score: 1.0,
            timestamp: 1000,
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
            source: None,
            created_by: None,
            updated_at: None,
            valid_from: None,
            valid_until: None,
            is_latest: true,
            superseded_by: None,
            supersedes_id: None,
            root_memory_id: None,
            version,
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
            provenance_role: crate::storage::ProvenanceRole::Claim,
        }
    }

    /// Link `old` → `new` the way `apply_supersede` does, without the store
    /// round-trip: old loses `is_latest`, new points back and forward.
    fn link(old: &mut Chunk, new: &mut Chunk) {
        old.is_latest = false;
        old.superseded_by = Some(new.id.clone());
        new.supersedes_id = Some(old.id.clone());
        new.root_memory_id = Some(old.root_memory_id.clone().unwrap_or_else(|| old.id.clone()));
    }

    fn ids(chain: &[Chunk]) -> Vec<&str> {
        chain.iter().map(|c| c.id.as_str()).collect()
    }

    /// Linear chain v1 → v2 → v3: every end yields the same root→latest walk
    /// (the pre-brief behaviour, preserved).
    #[test]
    fn chain_linear_same_from_any_version() {
        let (_tmp, store) = chain_store();
        let mut v1 = chain_chunk("v1", 1);
        let mut v2 = chain_chunk("v2", 2);
        let mut v3 = chain_chunk("v3", 3);
        link(&mut v1, &mut v2);
        link(&mut v2, &mut v3);
        for c in [&v1, &v2, &v3] {
            store.store_chunk(c).expect("store");
        }
        for id in ["v1", "v2", "v3"] {
            let chain = get_memory_chain(&store, id, 20).expect("chain");
            assert_eq!(ids(&chain), vec!["v1", "v2", "v3"], "queried {id}");
        }
        // `limit` is a window anchored on the queried id: ancestors first,
        // then successors — and it bounds the walk, not just the output.
        assert_eq!(
            ids(&get_memory_chain(&store, "v1", 2).expect("chain")),
            vec!["v1", "v2"]
        );
        assert_eq!(
            ids(&get_memory_chain(&store, "v3", 2).expect("chain")),
            vec!["v2", "v3"]
        );
        assert_eq!(
            ids(&get_memory_chain(&store, "v2", 1).expect("chain")),
            vec!["v2"]
        );
        assert!(get_memory_chain(&store, "v2", 0).expect("chain").is_empty());
    }

    /// `link_successor` sets exactly the chain fields `apply_supersede` does,
    /// inheriting the root and bumping the version.
    #[test]
    fn link_successor_sets_chain_fields() {
        let mut root = chain_chunk("root", 1);
        let v2 = link_successor(&root, chain_chunk("v2", 1));
        assert_eq!(v2.supersedes_id.as_deref(), Some("root"));
        assert_eq!(v2.root_memory_id.as_deref(), Some("root"));
        assert_eq!(v2.version, 2);

        root.root_memory_id = Some("elsewhere".to_string());
        root.version = 7;
        let v8 = link_successor(&root, chain_chunk("v8", 1));
        assert_eq!(v8.root_memory_id.as_deref(), Some("elsewhere"));
        assert_eq!(v8.version, 8);
    }

    /// B1: a soft-deleted version is invisible from every end of the chain,
    /// while the surviving versions stay linked through it.
    #[test]
    fn chain_hides_soft_deleted_versions() {
        let (_tmp, store) = chain_store();
        let mut v1 = chain_chunk("v1", 1);
        let mut v2 = chain_chunk("v2", 2);
        let mut v3 = chain_chunk("v3", 3);
        link(&mut v1, &mut v2);
        link(&mut v2, &mut v3);
        v2.deleted_at = Some(2000);
        for c in [&v1, &v2, &v3] {
            store.store_chunk(c).expect("store");
        }
        for id in ["v1", "v2", "v3"] {
            let chain = get_memory_chain(&store, id, 20).expect("chain");
            assert_eq!(ids(&chain), vec!["v1", "v3"], "queried {id}");
            assert!(
                chain.iter().all(|c| c.content != "content for v2"),
                "deleted content must not resurface (queried {id})"
            );
        }
    }

    /// B1: deleting the head leaves the older version(s); deleting every
    /// version yields an empty chain rather than a tombstone with content.
    #[test]
    fn chain_deleted_head_and_fully_deleted_chain() {
        let (_tmp, store) = chain_store();
        let mut v1 = chain_chunk("v1", 1);
        let mut v2 = chain_chunk("v2", 2);
        link(&mut v1, &mut v2);
        v2.deleted_at = Some(2000);
        store.store_chunk(&v1).expect("store");
        store.store_chunk(&v2).expect("store");
        assert_eq!(
            ids(&get_memory_chain(&store, "v2", 20).expect("chain")),
            vec!["v1"]
        );
        assert_eq!(
            ids(&get_memory_chain(&store, "v1", 20).expect("chain")),
            vec!["v1"]
        );

        v1.deleted_at = Some(2001);
        store.store_chunk(&v1).expect("store");
        assert!(get_memory_chain(&store, "v1", 20)
            .expect("chain")
            .is_empty());
        assert!(get_memory_chain(&store, "v2", 20)
            .expect("chain")
            .is_empty());
    }

    /// B2: two successors of one chunk (a branch). The root's `superseded_by`
    /// points at the last writer, but each successor's history must contain
    /// itself — the old root-first walk returned root → last writer for both.
    #[test]
    fn chain_branch_follows_the_queried_branch() {
        let (_tmp, store) = chain_store();
        let mut root = chain_chunk("root", 1);
        let mut first = chain_chunk("first", 2);
        let mut second = chain_chunk("second", 2);
        link(&mut root, &mut first);
        link(&mut root, &mut second); // overwrites root.superseded_by → "second"
        for c in [&root, &first, &second] {
            store.store_chunk(c).expect("store");
        }
        assert_eq!(
            ids(&get_memory_chain(&store, "first", 20).expect("chain")),
            vec!["root", "first"]
        );
        assert_eq!(
            ids(&get_memory_chain(&store, "second", 20).expect("chain")),
            vec!["root", "second"]
        );
        // The root itself follows its own forward pointer (last writer).
        assert_eq!(
            ids(&get_memory_chain(&store, "root", 20).expect("chain")),
            vec!["root", "second"]
        );
    }

    /// Unknown ids and pointer cycles terminate with what could be resolved.
    #[test]
    fn chain_unknown_id_and_cycle_terminate() {
        let (_tmp, store) = chain_store();
        assert!(get_memory_chain(&store, "nope", 20)
            .expect("chain")
            .is_empty());

        let mut a = chain_chunk("a", 1);
        let mut b = chain_chunk("b", 2);
        link(&mut a, &mut b);
        a.supersedes_id = Some("b".to_string()); // corrupt: b → a → b
        store.store_chunk(&a).expect("store");
        store.store_chunk(&b).expect("store");
        let chain = get_memory_chain(&store, "b", 20).expect("chain");
        assert_eq!(chain.len(), 2);
        assert!(chain.iter().any(|c| c.id == "b"));
    }
}
