//! Server-side glue for stream-kind-aware profiles/manifests (cycle/139).
//!
//! The domain (`loomem_core::manifest`) is HTTP-agnostic: it talks to the LLM
//! through the [`ManifestCompleter`] trait. This module supplies the production
//! implementation ([`HttpManifestCompleter`], wrapping `reqwest` + the OpenAI
//! chat endpoint) and the [`build_profile_or_manifest`] routing helper so the
//! MCP dispatcher and the context-pack handler stay thin orchestrators
//! (CLAUDE.md §4, ADR-014).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use loomem_core::config::LlmConfig;
use loomem_core::manifest::{classify_stream, ManifestCompleter, ProfileOrManifest, StreamKind};

use crate::AppState;

/// Production [`ManifestCompleter`] — one OpenAI chat completion. Mirrors the
/// reqwest call in `loomem_core::profile::generate_profile`, but lives in the
/// server layer so the domain stays free of `reqwest` (the trait is the seam).
pub(crate) struct HttpManifestCompleter {
    client: reqwest::Client,
    api_key: Option<String>,
    model: String,
    timeout_secs: u64,
}

impl HttpManifestCompleter {
    pub(crate) fn new(client: reqwest::Client, llm: &LlmConfig, model: String) -> Self {
        Self {
            client,
            api_key: llm.get_api_key(),
            model,
            timeout_secs: llm.timeout_secs,
        }
    }
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}
#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}
#[derive(Deserialize)]
struct ChatMessage {
    content: String,
}

impl ManifestCompleter for HttpManifestCompleter {
    fn complete(&self, prompt: &str) -> impl std::future::Future<Output = Result<String>> + Send {
        // Clone everything the future needs so it owns its captures (Send + no
        // borrow held across the await).
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let model = self.model.clone();
        let timeout = self.timeout_secs;
        let prompt = prompt.to_string();

        async move {
            let api_key =
                api_key.context("OpenAI API key not configured for manifest generation")?;
            let request_body = serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": prompt}],
                "max_tokens": 1000,
                "temperature": 0.0
            });

            let response = client
                .post("https://api.openai.com/v1/chat/completions")
                .header("Authorization", format!("Bearer {}", api_key))
                .header("Content-Type", "application/json")
                .json(&request_body)
                .timeout(Duration::from_secs(timeout))
                .send()
                .await
                .context("Failed to send manifest generation request")?;

            let status = response.status();
            if !status.is_success() {
                let error_text = response.text().await.unwrap_or_default();
                anyhow::bail!("Manifest LLM call failed ({}): {}", status, error_text);
            }

            let parsed: ChatResponse = response
                .json()
                .await
                .context("Failed to parse manifest LLM response")?;
            Ok(parsed
                .choices
                .into_iter()
                .next()
                .map(|c| c.message.content)
                .unwrap_or_default())
        }
    }
}

/// Route a stream to either a private profile or a knowledge-base manifest.
///
/// `stream_id` is authoritative (ADR-014): private streams keep the untouched
/// `UserProfile` path; shared/project streams get a `StreamManifest`. Returning
/// the [`ProfileOrManifest`] enum keeps the call sites trivial — they just
/// `to_markdown()` or serialise.
pub(crate) async fn build_profile_or_manifest(
    state: &Arc<AppState>,
    stream: &str,
    force_refresh: bool,
) -> Result<ProfileOrManifest> {
    match classify_stream(stream) {
        StreamKind::Private => {
            let profile = loomem_core::profile::get_or_generate_profile(
                &state.http_client,
                &state.config.llm,
                &state.config.profile,
                &state.store,
                stream,
                &state.config.storage.data_dir,
                force_refresh,
            )
            .await?;
            Ok(ProfileOrManifest::Profile(profile))
        }
        StreamKind::Shared | StreamKind::Project => {
            let completer = HttpManifestCompleter::new(
                state.http_client.clone(),
                &state.config.llm,
                state.config.manifest.model.clone(),
            );
            let manifest = loomem_core::manifest::get_or_generate_manifest(
                &completer,
                &state.config.manifest,
                &state.store,
                stream,
                &state.config.storage.data_dir,
                force_refresh,
            )
            .await?;
            Ok(ProfileOrManifest::Manifest(manifest))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loomem_core::storage::Chunk;

    /// Minimal live chunk authored by `created_by` in `stream`. Real store, no
    /// mock (CLAUDE.md §6).
    fn chunk(id: &str, stream: &str, content: &str, created_by: &str) -> Chunk {
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

    // AC-2: memory_profile on a shared stream with chunks from ≥2 people returns
    // a manifest (knowledge base), never a person profile. Manifest LLM is
    // disabled in the test config → no HTTP; routing + no-identity is the SUT.
    #[tokio::test]
    async fn ac2_shared_stream_routes_to_manifest() {
        let (_app, state) = crate::tests::make_test_app();
        let stream = "__shared_test139_ac2";
        state
            .store
            .store_chunk(&chunk(
                "c1",
                stream,
                "Anna migrated search to Tantivy.",
                "anna",
            ))
            .unwrap();
        state
            .store
            .store_chunk(&chunk(
                "c2",
                stream,
                "Bartek shipped Stripe billing.",
                "bartek",
            ))
            .unwrap();

        let result = build_profile_or_manifest(&state, stream, true)
            .await
            .expect("manifest path should succeed without LLM (enabled=false)");

        match result {
            ProfileOrManifest::Manifest(ref m) => {
                assert_eq!(m.kind, StreamKind::Shared);
                assert_eq!(m.stats.memory_count, 2);
            }
            ProfileOrManifest::Profile(_) => {
                panic!("shared stream must not return a person profile")
            }
        }

        let md = result.to_markdown();
        assert!(md.contains("# Knowledge Base"), "md: {md}");
        assert!(
            !md.contains("### Identity"),
            "shared stream leaked person identity: {md}"
        );

        // JSON form (format=json) also carries no identity field.
        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains("\"identity\""), "json: {json}");
    }

    // AC-3 (safety): a private stream is NEVER turned into a manifest. The
    // private path delegates to the untouched UserProfile pipeline; without an
    // API key generation errors, but it must never silently become a manifest.
    #[tokio::test]
    async fn ac3_private_stream_never_returns_manifest() {
        let (_app, state) = crate::tests::make_test_app();
        let stream = "__user_test139_ac3";
        state
            .store
            .store_chunk(&chunk("p1", stream, "I prefer lean dependencies.", "anna"))
            .unwrap();

        let result = build_profile_or_manifest(&state, stream, true).await;
        // Either a profile (if generation succeeded) or an error — but never a
        // manifest. classify_stream(private) must route away from the manifest path.
        assert!(
            !matches!(result, Ok(ProfileOrManifest::Manifest(_))),
            "private stream was routed to a manifest"
        );
        assert_eq!(classify_stream(stream), StreamKind::Private);
    }
}
