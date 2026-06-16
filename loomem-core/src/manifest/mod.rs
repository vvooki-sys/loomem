//! Stream-kind-aware manifest synthesis (ADR-014, cycle/139).
//!
//! `memory_profile` / `memory_context` historically synthesised a single
//! `UserProfile` for every stream — correct for private streams
//! (`__user_<uuid>`), but semantically wrong for shared (`__shared_*`) and
//! project (`__project_*`) streams, where it returned one person's profile as
//! the profile of a whole knowledge base.
//!
//! This module adds the alternative path: a [`StreamManifest`] — a dossier of
//! a knowledge base, not a person. It is a deterministic/LLM hybrid:
//!
//! - **Governance** (`title`, `purpose`, scope, operators, source-of-truth) is
//!   declarative, from [`config::ManifestConfig`] — never LLM-generated, so a
//!   load-bearing rule can't be hallucinated (ADR-014 alt. D).
//! - **Contents** (`contents_summary`, `topic_clusters`) is LLM-generated from
//!   the stream's chunks, in a collective framing (never a person).
//! - **Stats** are computed deterministically from the store.
//!
//! The private (`UserProfile`) path is untouched (CLAUDE.md §0.5.3): see
//! [`crate::profile`].

pub mod config;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::debug;

pub use config::{ManifestConfig, StreamGovernance};

use crate::profile::UserProfile;
use crate::storage::RocksDbStore;

#[cfg(test)]
mod tests;

/// Classification of a stream. `stream_id` is authoritative, not access level:
/// an `access=admin` caller on a shared stream still gets a manifest, not their
/// own profile (ADR-014).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamKind {
    Private,
    Shared,
    Project,
}

/// Deterministic classifier. `stream_id` is authoritative (not access level).
///
/// §1 carve-out does NOT apply — this has branches; kept at CC ≤ 10.
pub fn classify_stream(stream_id: &str) -> StreamKind {
    if stream_id.starts_with("__shared_") {
        StreamKind::Shared
    } else if stream_id.starts_with("__project_") {
        StreamKind::Project
    } else {
        StreamKind::Private
    }
}

/// Deterministic statistics about a stream's contents.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManifestStats {
    pub memory_count: usize,
    /// Instance-global embedding count (not per-stream — see `generate_manifest`).
    pub embedding_count: usize,
    /// Instance-global associator cluster count (not per-stream).
    pub cluster_count: usize,
    pub last_updated: u64,
}

/// A knowledge-base dossier for a shared/project stream. Deliberately has no
/// `identity`/person fields (contrast [`UserProfile`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamManifest {
    pub stream_id: String,
    pub kind: StreamKind,
    // --- declarative (from ManifestConfig.streams[stream_id]); empty when absent ---
    pub title: String,
    pub purpose: String,
    pub scope_includes: String,
    pub scope_excludes: String,
    pub governance: String,
    pub source_of_truth: String,
    // --- LLM (collective framing, never a person) ---
    pub contents_summary: String,
    pub topic_clusters: Vec<String>,
    // --- deterministic ---
    pub stats: ManifestStats,
    pub governance_configured: bool,
    pub generated_at: u64,
}

/// LLM seam (DI, CLAUDE.md §4). The production implementation lives in
/// `loomem-server` and wraps `reqwest`; tests inject a deterministic stub so
/// no real HTTP happens (AC-7). Uses native async-fn-in-trait rather than the
/// `async-trait` crate to avoid a new dependency (CLAUDE.md §7) — the design
/// intent (trait-based DI, stub-testable) from ADR-014 is preserved.
pub trait ManifestCompleter: Send + Sync {
    fn complete(&self, prompt: &str) -> impl std::future::Future<Output = Result<String>> + Send;
}

/// Either a private profile or a knowledge-base manifest. Lets handlers stay
/// trivial: serialise (untagged → the inner struct) for `format=json`, or call
/// [`ProfileOrManifest::to_markdown`].
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ProfileOrManifest {
    Profile(UserProfile),
    Manifest(StreamManifest),
}

impl ProfileOrManifest {
    /// Render to markdown. The private branch delegates to the untouched
    /// [`crate::profile::profile_to_markdown`], so private output is identical
    /// to the pre-/139 rendering (AC-3).
    pub fn to_markdown(&self) -> String {
        match self {
            ProfileOrManifest::Profile(p) => crate::profile::profile_to_markdown(p),
            ProfileOrManifest::Manifest(m) => manifest_to_markdown(m),
        }
    }
}

const MANIFEST_PROMPT: &str = r#"You describe a SHARED organizational memory stream — a knowledge base used by many people. This is NOT a person. NEVER produce a personal profile, name, or `identity` field. Describe the stream, not any individual.

You receive a sample of memories from the stream. Output ONLY a JSON object:
{
  "contents_summary": "2-4 sentences: what kind of knowledge this stream collectively holds (topics, domains, decisions). Describe the corpus, never a single author.",
  "topic_clusters": ["recurring topic or domain", "another recurring topic"]
}

RULES:
- Collective framing only. Write "the stream contains…", never "the user…".
- NO names, NO personal identity, NO per-person profile.
- Language: match the dominant language of the memories.
- Return ONLY the JSON object, no markdown, no code fences, no preamble."#;

/// Generate a manifest for a shared/project stream. Pure synthesis — caching is
/// handled by [`get_or_generate_manifest`].
///
/// When `config.enabled == false` (or the stream has no chunks) the LLM step is
/// skipped entirely: `contents_summary`/`topic_clusters` stay empty and no HTTP
/// is performed. Governance + stats are always populated.
pub async fn generate_manifest<C: ManifestCompleter>(
    completer: &C,
    config: &ManifestConfig,
    store: &RocksDbStore,
    stream: &str,
) -> Result<StreamManifest> {
    let kind = classify_stream(stream);
    let gov = config.streams.get(stream);
    let chunks = collect_stream_chunks(store, stream);
    let stats = compute_stats(store, &chunks);
    let contents = top_contents(chunks, config.max_chunks);

    let (contents_summary, topic_clusters) = if config.enabled && !contents.is_empty() {
        generate_contents(completer, &contents).await?
    } else {
        (String::new(), Vec::new())
    };

    Ok(StreamManifest {
        stream_id: stream.to_string(),
        kind,
        title: gov.map(|g| g.title.clone()).unwrap_or_default(),
        purpose: gov.map(|g| g.purpose.clone()).unwrap_or_default(),
        scope_includes: gov.map(|g| g.scope_includes.clone()).unwrap_or_default(),
        scope_excludes: gov.map(|g| g.scope_excludes.clone()).unwrap_or_default(),
        governance: gov.map(|g| g.governance.clone()).unwrap_or_default(),
        source_of_truth: gov.map(|g| g.source_of_truth.clone()).unwrap_or_default(),
        contents_summary,
        topic_clusters,
        stats,
        governance_configured: gov.is_some(),
        generated_at: now_secs(),
    })
}

/// Collect this stream's live chunks (latest, not dormant, not soft-deleted),
/// mirroring the filter in `profile::generate_profile`.
fn collect_stream_chunks(store: &RocksDbStore, stream: &str) -> Vec<crate::storage::Chunk> {
    let mut chunks = Vec::new();
    for level in 0..=1 {
        let prefix = format!("chunk:L{}:", level);
        for (_key, value) in store.prefix_scan(prefix.as_bytes()) {
            if let Ok(chunk) = store.decode_chunk(&value) {
                if chunk.stream == stream
                    && chunk.is_latest
                    && !chunk.dormant
                    && chunk.deleted_at.is_none()
                {
                    chunks.push(chunk);
                }
            }
        }
    }
    chunks
}

/// Deterministic stats. `memory_count`/`last_updated` are per-stream; embedding
/// and cluster counts are instance-global (matching `memory_status`) to avoid
/// re-deriving per-stream correlations the associator already owns (ADR-014 /
/// brief: "nie dubluj associatora").
fn compute_stats(store: &RocksDbStore, chunks: &[crate::storage::Chunk]) -> ManifestStats {
    ManifestStats {
        memory_count: chunks.len(),
        embedding_count: store.count_embeddings().unwrap_or(0),
        cluster_count: store.prefix_scan(b"assoc:centroid:").count(),
        last_updated: chunks.iter().map(|c| c.timestamp).max().unwrap_or(0),
    }
}

/// Top-N chunk contents by importance then recency (mirrors `generate_profile`).
fn top_contents(mut chunks: Vec<crate::storage::Chunk>, max_chunks: usize) -> Vec<String> {
    chunks.sort_by(|a, b| {
        let imp_a = a.importance.unwrap_or(0.0);
        let imp_b = b.importance.unwrap_or(0.0);
        imp_b
            .partial_cmp(&imp_a)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.timestamp.cmp(&a.timestamp))
    });
    chunks.truncate(max_chunks);
    chunks.into_iter().map(|c| c.content).collect()
}

/// Run the contents-summary completion and parse the JSON result.
async fn generate_contents<C: ManifestCompleter>(
    completer: &C,
    contents: &[String],
) -> Result<(String, Vec<String>)> {
    let joined = contents.join("\n");
    let prompt = format!("{}\n\n=== STREAM CONTENTS ===\n{}", MANIFEST_PROMPT, joined);
    let raw = completer
        .complete(&prompt)
        .await
        .context("manifest contents completion failed")?;
    Ok(parse_contents_json(&raw))
}

#[derive(Deserialize, Default)]
struct ContentsJson {
    #[serde(default)]
    contents_summary: String,
    #[serde(default)]
    topic_clusters: Vec<String>,
}

/// Parse the LLM's JSON, tolerating fences/preamble. Degrades to empty content
/// on failure (the manifest still renders governance + stats) rather than
/// erroring the whole call.
fn parse_contents_json(raw: &str) -> (String, Vec<String>) {
    let trimmed = raw.trim().trim_start_matches('\u{feff}');
    let stripped = trimmed
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    if let Ok(v) = serde_json::from_str::<ContentsJson>(stripped) {
        return (v.contents_summary, v.topic_clusters);
    }
    if let (Some(s), Some(e)) = (stripped.find('{'), stripped.rfind('}')) {
        if let Ok(v) = serde_json::from_str::<ContentsJson>(&stripped[s..=e]) {
            return (v.contents_summary, v.topic_clusters);
        }
    }
    (String::new(), Vec::new())
}

/// Render a manifest as markdown for system-prompt injection / MCP output.
pub fn manifest_to_markdown(m: &StreamManifest) -> String {
    let mut md = String::new();
    let title = if m.title.is_empty() {
        m.stream_id.as_str()
    } else {
        m.title.as_str()
    };
    md.push_str(&format!("# Knowledge Base: {}\n", title));

    if m.governance_configured {
        push_governance_sections(&mut md, m);
    } else {
        md.push_str("\n⚠ Governance not configured for this stream; describing contents only.\n");
    }

    push_contents_section(&mut md, m);
    push_stats_section(&mut md, &m.stats);
    md
}

fn push_governance_sections(md: &mut String, m: &StreamManifest) {
    let sections = [
        ("Purpose", &m.purpose),
        ("Scope — includes", &m.scope_includes),
        ("Scope — excludes", &m.scope_excludes),
        ("Governance", &m.governance),
        ("Source of truth", &m.source_of_truth),
    ];
    for (title, content) in sections {
        if !content.is_empty() {
            md.push_str(&format!("\n## {}\n\n{}\n", title, content));
        }
    }
}

fn push_contents_section(md: &mut String, m: &StreamManifest) {
    if !m.contents_summary.is_empty() {
        md.push_str(&format!("\n## Contents\n\n{}\n", m.contents_summary));
    }
    if !m.topic_clusters.is_empty() {
        let list = m
            .topic_clusters
            .iter()
            .map(|t| format!("- {}", t))
            .collect::<Vec<_>>()
            .join("\n");
        md.push_str(&format!("\n## Topic clusters\n\n{}\n", list));
    }
}

fn push_stats_section(md: &mut String, s: &ManifestStats) {
    md.push_str(&format!(
        "\n## Statistics\n\n- Memories: {}\n- Embeddings (instance-global): {}\n- Clusters (instance-global): {}\n- Last updated: {}\n",
        s.memory_count, s.embedding_count, s.cluster_count, s.last_updated
    ));
}

/// Cached-or-generated manifest. Mirrors `profile::get_or_generate_profile`:
/// file cache under `data_dir/manifests/`, TTL + `manifest_dirty:<stream>` flag.
pub async fn get_or_generate_manifest<C: ManifestCompleter>(
    completer: &C,
    config: &ManifestConfig,
    store: &RocksDbStore,
    stream: &str,
    data_dir: &Path,
    force_refresh: bool,
) -> Result<StreamManifest> {
    let cache_path = manifest_cache_path(data_dir, stream);

    if !force_refresh {
        if let Some(cached) = load_cached_manifest(&cache_path, config.cache_ttl_secs)? {
            let dirty_key = format!("manifest_dirty:{}", stream);
            if store.get(dirty_key.as_bytes())?.is_none() {
                debug!("Returning cached manifest for stream {}", stream);
                return Ok(cached);
            }
            debug!("Manifest cache dirty for stream {}, regenerating", stream);
        }
    }

    let manifest = generate_manifest(completer, config, store, stream).await?;
    save_manifest_cache(&cache_path, &manifest)?;

    let dirty_key = format!("manifest_dirty:{}", stream);
    let _ = store.delete(dirty_key.as_bytes());

    Ok(manifest)
}

/// Set the dirty flag for a stream's manifest cache (cache invalidation on
/// ingest). Sibling of `profile::mark_profile_dirty`.
pub fn mark_manifest_dirty(store: &RocksDbStore, stream: &str) -> Result<()> {
    let dirty_key = format!("manifest_dirty:{}", stream);
    store.put(dirty_key.as_bytes(), b"1")?;
    Ok(())
}

fn manifest_cache_path(data_dir: &Path, stream: &str) -> PathBuf {
    data_dir
        .join("manifests")
        .join(format!("stream_{}.json", stream))
}

fn load_cached_manifest(path: &Path, ttl_secs: u64) -> Result<Option<StreamManifest>> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path).context("Failed to read manifest cache")?;
    let manifest: StreamManifest = match serde_json::from_str(&content) {
        Ok(m) => m,
        Err(_) => {
            debug!("Manifest cache format mismatch, treating as expired");
            return Ok(None);
        }
    };
    let age = now_secs().saturating_sub(manifest.generated_at);
    if age > ttl_secs {
        return Ok(None);
    }
    Ok(Some(manifest))
}

fn save_manifest_cache(path: &Path, manifest: &StreamManifest) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("Failed to create manifests directory")?;
    }
    let json = serde_json::to_string_pretty(manifest).context("Failed to serialize manifest")?;
    std::fs::write(path, json).context("Failed to write manifest cache")?;
    Ok(())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
