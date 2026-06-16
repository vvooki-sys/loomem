//! Tests for stream-kind-aware manifest synthesis (cycle/139, ADR-014).
//!
//! Storage is a real tmpdir RocksDB (`fresh_store`), never a mock (CLAUDE.md
//! §6). The LLM is injected as a deterministic [`StubCompleter`] so no real
//! HTTP happens (AC-7).

use super::*;
use crate::storage::{Chunk, RocksDbConfig, RocksDbStore};
use std::collections::HashMap;
use tempfile::TempDir;

// ── fixtures ──────────────────────────────────────────────────────

fn test_db_config() -> RocksDbConfig {
    RocksDbConfig {
        max_open_files: 100,
        compression: "none".to_string(),
        write_buffer_size: 4 * 1024 * 1024,
        max_write_buffer_number: 2,
    }
}

/// Fresh tmpdir-backed store. The `TempDir` is returned so the caller keeps it
/// alive (the store holds an open handle on the directory).
fn fresh_store() -> (RocksDbStore, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let store = RocksDbStore::open(tmp.path(), &test_db_config()).expect("open store");
    (store, tmp)
}

/// Minimal chunk in `stream`, authored by `created_by`, with `content`.
fn make_chunk(id: &str, stream: &str, content: &str, created_by: &str) -> Chunk {
    Chunk {
        id: id.to_string(),
        content: content.to_string(),
        stream: stream.to_string(),
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
        importance: Some(1.0),
        persistent: false,
        last_implicit_boost: None,
        access_count: 0,
        source: None,
        created_by: Some(created_by.to_string()),
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

/// Deterministic completer — returns a fixed JSON string, never touches HTTP.
struct StubCompleter(String);

impl ManifestCompleter for StubCompleter {
    fn complete(&self, _prompt: &str) -> impl std::future::Future<Output = Result<String>> + Send {
        let response = self.0.clone();
        async move { Ok(response) }
    }
}

fn enabled_config(streams: HashMap<String, StreamGovernance>) -> ManifestConfig {
    ManifestConfig {
        enabled: true,
        streams,
        ..Default::default()
    }
}

fn sample_governance() -> StreamGovernance {
    StreamGovernance {
        title: "Team Knowledge Base".to_string(),
        purpose: "Shared organizational memory for the team".to_string(),
        scope_includes: "Team decisions, project context".to_string(),
        scope_excludes: "Personal/private notes".to_string(),
        governance: "Admins: operators only".to_string(),
        source_of_truth: "knowledge-base import".to_string(),
    }
}

// ── AC-1: classify_stream ─────────────────────────────────────────

#[test]
fn ac1_classify_stream_all_kinds() {
    // Shared: __shared_ prefix.
    assert_eq!(classify_stream("__shared_team__"), StreamKind::Shared);
    assert_eq!(classify_stream("__shared_x"), StreamKind::Shared);
    // Project.
    assert_eq!(classify_stream("__project_abc"), StreamKind::Project);
    // Private: __user_ (including the default stream), bare numeric, and legacy ids.
    assert_eq!(
        classify_stream(crate::storage::DEFAULT_STREAM_ID),
        StreamKind::Private
    );
    assert_eq!(
        classify_stream("__user_0c8f1e2a-1111-2222-3333-444455556666"),
        StreamKind::Private
    );
    assert_eq!(classify_stream("100"), StreamKind::Private);
    assert_eq!(classify_stream("personal"), StreamKind::Private);
}

// ── AC-2 (core): shared manifest is a knowledge base, not a person ─

#[tokio::test]
async fn ac2_shared_manifest_has_no_identity() {
    let (store, _tmp) = fresh_store();
    // Chunks from two different "people" — the bug returned one of them as the
    // stream's identity.
    store
        .store_chunk(&make_chunk(
            "c1",
            "__shared_team__",
            "Anna decided to migrate the search pipeline to Tantivy.",
            "anna",
        ))
        .unwrap();
    store
        .store_chunk(&make_chunk(
            "c2",
            "__shared_team__",
            "Bartek shipped the billing integration on Stripe.",
            "bartek",
        ))
        .unwrap();

    let mut streams = HashMap::new();
    streams.insert("__shared_team__".to_string(), sample_governance());
    let config = enabled_config(streams);

    let stub = StubCompleter(
        r#"{"contents_summary":"The stream holds team decisions about search and billing.","topic_clusters":["search","billing"]}"#
            .to_string(),
    );

    let manifest = generate_manifest(&stub, &config, &store, "__shared_team__")
        .await
        .unwrap();

    // It is a knowledge base manifest.
    assert_eq!(manifest.kind, StreamKind::Shared);
    assert!(manifest.governance_configured);
    assert_eq!(manifest.title, "Team Knowledge Base");
    assert_eq!(manifest.stats.memory_count, 2);
    assert_eq!(
        manifest.contents_summary,
        "The stream holds team decisions about search and billing."
    );
    assert_eq!(manifest.topic_clusters, vec!["search", "billing"]);

    // JSON serialization has NO `identity` field and no person name.
    let json = serde_json::to_string(&manifest).unwrap();
    assert!(
        !json.contains("\"identity\""),
        "manifest must not have identity: {json}"
    );

    // Markdown is a knowledge-base dossier, not a person profile.
    let md = manifest_to_markdown(&manifest);
    assert!(md.contains("# Knowledge Base: Team Knowledge Base"));
    assert!(md.contains("## Governance"));
    assert!(md.contains("## Contents"));
    assert!(
        !md.contains("### Identity"),
        "markdown must not render person identity: {md}"
    );
}

// ── AC-5: no governance entry → warning, never a person ───────────

#[tokio::test]
async fn ac5_shared_no_governance_degrades_to_minimum() {
    let (store, _tmp) = fresh_store();
    store
        .store_chunk(&make_chunk(
            "c1",
            "__shared_orphan",
            "Some shared note.",
            "anna",
        ))
        .unwrap();

    // No governance entry for this stream; LLM disabled (no HTTP).
    let config = ManifestConfig::default();
    let stub = StubCompleter(String::new());

    let manifest = generate_manifest(&stub, &config, &store, "__shared_orphan")
        .await
        .unwrap();

    assert_eq!(manifest.kind, StreamKind::Shared);
    assert!(!manifest.governance_configured);
    assert!(manifest.title.is_empty());
    assert!(manifest.contents_summary.is_empty()); // LLM skipped (enabled=false)
    assert_eq!(manifest.stats.memory_count, 1);

    let md = manifest_to_markdown(&manifest);
    assert!(md.contains("⚠ Governance not configured"));
    assert!(!md.contains("### Identity"));
    // Falls back to stream_id as title, never a person.
    assert!(md.contains("# Knowledge Base: __shared_orphan"));
}

// ── AC-6: ManifestConfig with #[serde(default)] on the root field ──

#[test]
fn ac6_config_without_manifest_section_uses_default() {
    #[derive(serde::Deserialize)]
    struct Wrapper {
        #[serde(default)]
        manifest: ManifestConfig,
    }

    // Input lacks `manifest` entirely — mirrors a config.toml without [manifest].
    let w: Wrapper = serde_json::from_str("{}").unwrap();
    assert!(!w.manifest.enabled);
    assert_eq!(w.manifest.model, "gpt-4.1-mini");
    assert_eq!(w.manifest.max_chunks, 100);
    assert_eq!(w.manifest.cache_ttl_secs, 3600);
    assert!(w.manifest.streams.is_empty());

    // A fully-specified section round-trips, including per-stream governance.
    let json = r#"{"manifest":{"enabled":true,"model":"m","max_chunks":5,"cache_ttl_secs":10,
        "streams":{"__shared_x":{"title":"T","purpose":"P","scope_includes":"I",
        "scope_excludes":"E","governance":"G","source_of_truth":"S"}}}}"#;
    let w: Wrapper = serde_json::from_str(json).unwrap();
    assert!(w.manifest.enabled);
    assert!(w.manifest.streams.contains_key("__shared_x"));
    assert_eq!(w.manifest.streams["__shared_x"].title, "T");
}

// ── AC-7: LLM via trait, deterministic stub, zero HTTP ────────────

#[tokio::test]
async fn ac7_llm_via_stub_completer_no_http() {
    let (store, _tmp) = fresh_store();
    store
        .store_chunk(&make_chunk(
            "c1",
            "__project_alpha",
            "Project note.",
            "anna",
        ))
        .unwrap();

    let config = enabled_config(HashMap::new());
    let stub = StubCompleter(
        r#"{"contents_summary":"Notes about project alpha.","topic_clusters":["alpha"]}"#
            .to_string(),
    );

    let manifest = generate_manifest(&stub, &config, &store, "__project_alpha")
        .await
        .unwrap();

    assert_eq!(manifest.kind, StreamKind::Project);
    assert_eq!(manifest.contents_summary, "Notes about project alpha.");
    assert_eq!(manifest.topic_clusters, vec!["alpha"]);
}

// ── parse_contents_json tolerance ─────────────────────────────────

#[test]
fn parse_contents_json_handles_fences_and_garbage() {
    // Fenced JSON.
    let (s, t) = parse_contents_json(
        "```json\n{\"contents_summary\":\"x\",\"topic_clusters\":[\"a\"]}\n```",
    );
    assert_eq!(s, "x");
    assert_eq!(t, vec!["a"]);

    // Preamble + trailing prose around the object.
    let (s, _) =
        parse_contents_json("Sure! {\"contents_summary\":\"y\",\"topic_clusters\":[]} done");
    assert_eq!(s, "y");

    // Total garbage → empty, never panics.
    let (s, t) = parse_contents_json("not json at all");
    assert!(s.is_empty());
    assert!(t.is_empty());
}

// ── AC-3 (routing identity): Profile variant renders identically ──

#[test]
fn ac3_profile_variant_markdown_byte_identical() {
    let profile = UserProfile {
        identity: "Anna, Warsaw, runs Acme".to_string(),
        summary: "Founder of Acme.".to_string(),
        expertise: "Rust, memory systems".to_string(),
        ..Default::default()
    };
    let direct = crate::profile::profile_to_markdown(&profile);
    let via_enum = ProfileOrManifest::Profile(profile).to_markdown();
    // The routing layer must not alter private rendering at all (AC-3).
    assert_eq!(direct, via_enum);
    // And it is a person profile, not a manifest.
    assert!(via_enum.contains("### Identity"));
    assert!(!via_enum.contains("# Knowledge Base"));
}
