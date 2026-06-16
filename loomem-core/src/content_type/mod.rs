//! Content-type classification for search results (ADR-017, cycle/142 + /143).
//!
//! `ContentType` is a dimension **orthogonal to `FactType`**: it describes the
//! *form* of a document (policy / changelog / case-study / instruction / …), not
//! the form of an assertion.
//!
//! ## LLM-authoritative (ADR-017 Amendment v2, cycle/143)
//! The LLM (`HttpContentTypeClassifier`, trait-injected — ADR-014 seam) is the
//! **only** classifier, run **once at write time**. The earlier deterministic
//! pattern detector was removed: empirically it confidently mis-tags prose
//! (validation gate /142 — the verb "musi" → `policy`, any numbered list →
//! `operational_instruction`). `other` (returned by the prompt when the model is
//! unsure) is now the explicit uncertainty bucket, replacing the old confidence
//! band. `classify_content` returns `None` when typing is disabled or the LLM
//! call fails — it never guesses.
//!
//! ## Storage (ADR-017 Amendment)
//! The result is persisted in a **sidecar keyspace** `content_type:<chunk_id>`
//! (default CF, wzorzec `auto_abstract` cache /90) — **not** as a `Chunk` field.
//! This keeps the ~80 `Chunk { .. }` construction sites untouched and lets the
//! backfill write sidecar rows without ever rewriting (re-encrypting) a chunk.
//! Classify-once-persist-never-reclassify: search only hydrates the stored value
//! (the LLM's non-determinism is frozen at the single write moment).
//!
//! ## Organizational neutrality (ADR-017 §4, AC-11)
//! This module classifies content *form* only. No organization name, stream id,
//! or business policy lives here — that belongs to governance/manifest config.
//! The v1 prompt (org-neutral, language-agnostic) lives in `loomem-server`.

pub mod config;

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::storage::RocksDbStore;
pub use config::ContentTypeConfig;

/// Form of a document. Orthogonal to `FactType`. snake_case on the wire
/// (spójne z ADR-016 `*_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentType {
    OperationalInstruction,
    Policy,
    Changelog,
    CaseStudy,
    Article,
    PersonProfile,
    Index,
    OrgFact,
    TechnicalProject,
    Other,
}

/// Which path produced the classification. Since /143 the LLM is the only
/// classifier, so this carries a single variant — retained as a typed surface
/// marker (`content_type_source: "llm"`) and so that a stale /142
/// `source: "deterministic"` sidecar row fails to deserialize and reads as
/// "no tag" until the LLM backfill overwrites it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassifierSource {
    Llm,
}

impl ContentType {
    /// snake_case wire token. §1 carve-out: declarative table, CC=1.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OperationalInstruction => "operational_instruction",
            Self::Policy => "policy",
            Self::Changelog => "changelog",
            Self::CaseStudy => "case_study",
            Self::Article => "article",
            Self::PersonProfile => "person_profile",
            Self::Index => "index",
            Self::OrgFact => "org_fact",
            Self::TechnicalProject => "technical_project",
            Self::Other => "other",
        }
    }

    /// Parse a snake_case token (e.g. an LLM-returned label). `None` on an
    /// unrecognized token — caller decides the fallback. §1 carve-out: CC=1.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "operational_instruction" => Self::OperationalInstruction,
            "policy" => Self::Policy,
            "changelog" => Self::Changelog,
            "case_study" => Self::CaseStudy,
            "article" => Self::Article,
            "person_profile" => Self::PersonProfile,
            "index" => Self::Index,
            "org_fact" => Self::OrgFact,
            "technical_project" => Self::TechnicalProject,
            "other" => Self::Other,
            _ => return None,
        })
    }
}

impl ClassifierSource {
    /// Surface string (`llm`) for `content_type_source`.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Llm => "llm",
        }
    }
}

/// Sidecar-persisted value, serialized as JSON under `content_type:<chunk_id>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentTypeMeta {
    pub content_type: ContentType,
    pub source: ClassifierSource,
}

/// LLM classifier seam (DI). Native AFIT (RPITIT + Send), NIE async-trait (§7).
/// The production impl (`HttpContentTypeClassifier`) lives in `loomem-server`.
pub trait ContentTypeClassifier: Send + Sync {
    fn classify(
        &self,
        content: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<ContentType>> + Send;
}

/// Classify a document's *form* via the LLM (the sole authoritative classifier
/// since /143). Returns `None` — never a guess — when typing is disabled or the
/// LLM call fails; the caller then writes no sidecar row, so the chunk reads as
/// untagged (≈ `other`) and is reclassified by a later backfill.
pub async fn classify_content(
    classifier: &impl ContentTypeClassifier,
    config: &ContentTypeConfig,
    store: &RocksDbStore,
    content: &str,
) -> Option<ContentTypeMeta> {
    if !config.enabled {
        return None; // typing off → no sidecar entry
    }
    match classify_via_llm_cached(classifier, config, store, content).await {
        Ok(content_type) => Some(ContentTypeMeta {
            content_type,
            source: ClassifierSource::Llm,
        }),
        Err(e) => {
            warn!("content_type LLM classify failed: {e}");
            None // error → no entry (do NOT guess)
        }
    }
}

/// Cache-by-model LLM classification (wzorzec /90 — klucz zawiera model).
async fn classify_via_llm_cached(
    classifier: &impl ContentTypeClassifier,
    config: &ContentTypeConfig,
    store: &RocksDbStore,
    content: &str,
) -> anyhow::Result<ContentType> {
    let hash = content_hash(content);
    if let Some(cached) = llm_cache_get(store, &hash, &config.model) {
        return Ok(cached);
    }
    let content_type = classifier.classify(content).await?;
    llm_cache_put(store, &hash, &config.model, content_type);
    Ok(content_type)
}

fn content_hash(content: &str) -> String {
    blake3::hash(content.as_bytes()).to_hex().to_string()
}

// ── LLM classification cache (content_type_cache:<hash>:<model>) ──

const LLM_CACHE_PREFIX: &str = "content_type_cache:";

fn llm_cache_key(hash: &str, model: &str) -> Vec<u8> {
    format!("{LLM_CACHE_PREFIX}{hash}:{model}").into_bytes()
}

fn llm_cache_get(store: &RocksDbStore, hash: &str, model: &str) -> Option<ContentType> {
    match store.db().get(llm_cache_key(hash, model)) {
        Ok(Some(bytes)) => std::str::from_utf8(&bytes)
            .ok()
            .and_then(ContentType::parse),
        _ => None,
    }
}

fn llm_cache_put(store: &RocksDbStore, hash: &str, model: &str, content_type: ContentType) {
    if let Err(e) = store
        .db()
        .put(llm_cache_key(hash, model), content_type.as_str().as_bytes())
    {
        warn!("content_type LLM cache write failed: {e}");
    }
}

// ── Sidecar persistence (content_type:<chunk_id>, wzorzec auto_abstract /90) ──

const SIDECAR_PREFIX: &str = "content_type:";

fn sidecar_key(chunk_id: &str) -> Vec<u8> {
    format!("{SIDECAR_PREFIX}{chunk_id}").into_bytes()
}

/// Persist a classification for a chunk. Best-effort (like `cache_put`): a write
/// failure is logged, not propagated — the tag is additive/informational.
pub fn put_content_type(store: &RocksDbStore, chunk_id: &str, meta: &ContentTypeMeta) {
    match serde_json::to_vec(meta) {
        Ok(bytes) => {
            if let Err(e) = store.db().put(sidecar_key(chunk_id), bytes) {
                warn!("content_type sidecar write failed for {chunk_id}: {e}");
            }
        }
        Err(e) => warn!("content_type sidecar serialize failed for {chunk_id}: {e}"),
    }
}

/// Read a chunk's classification. `None` = no sidecar entry (legacy /
/// unclassified, or a stale /142 `deterministic` row that no longer
/// deserializes) → no tag.
#[must_use]
pub fn get_content_type(store: &RocksDbStore, chunk_id: &str) -> Option<ContentTypeMeta> {
    match store.db().get(sidecar_key(chunk_id)) {
        Ok(Some(bytes)) => serde_json::from_slice(&bytes).ok(),
        _ => None,
    }
}

/// Batch hydration by id (search result build). Returns only the hits.
#[must_use]
pub fn get_content_types(store: &RocksDbStore, ids: &[String]) -> HashMap<String, ContentTypeMeta> {
    ids.iter()
        .filter_map(|id| get_content_type(store, id).map(|meta| (id.clone(), meta)))
        .collect()
}

#[cfg(test)]
mod tests;
