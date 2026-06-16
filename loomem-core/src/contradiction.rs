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
}

impl Default for ContradictionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            similarity_threshold: 0.70,
            max_candidates: 5,
            model: "gpt-4.1-mini".to_string(),
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
pub fn apply_supersede(
    store: &RocksDbStore,
    old_chunk: &Chunk,
    mut new_chunk: Chunk,
) -> Result<Chunk> {
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
    new_chunk.supersedes_id = Some(old_chunk.id.clone());
    new_chunk.root_memory_id = Some(
        old_chunk
            .root_memory_id
            .clone()
            .unwrap_or_else(|| old_chunk.id.clone()),
    );
    new_chunk.version = old_chunk.version + 1;

    debug!(
        "Superseded chunk {} (v{}) → {} (v{})",
        old_chunk.id, old_chunk.version, new_chunk.id, new_chunk.version
    );

    Ok(new_chunk)
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

/// Get the full version chain for a chunk (root → v1 → v2 → ... → current).
pub fn get_memory_chain(store: &RocksDbStore, chunk_id: &str, limit: usize) -> Result<Vec<Chunk>> {
    let mut chain = Vec::new();

    // First, find the root by walking backwards
    let mut current_id = chunk_id.to_string();
    let mut visited = std::collections::HashSet::new();
    loop {
        if visited.contains(&current_id) {
            break; // cycle detection
        }
        visited.insert(current_id.clone());

        if let Some(chunk) = store.get_chunk(&current_id)? {
            if let Some(ref prev_id) = chunk.supersedes_id {
                current_id = prev_id.clone();
            } else {
                break; // reached root
            }
        } else {
            break;
        }
    }

    // Now walk forward from root
    let mut visited = std::collections::HashSet::new();
    loop {
        if chain.len() >= limit {
            break;
        }
        if visited.contains(&current_id) {
            break; // cycle detection
        }
        visited.insert(current_id.clone());

        if let Some(chunk) = store.get_chunk(&current_id)? {
            let next_id = chunk.superseded_by.clone();
            chain.push(chunk);
            if let Some(next) = next_id {
                current_id = next;
            } else {
                break; // reached latest
            }
        } else {
            break;
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
