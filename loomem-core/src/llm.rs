use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::llm_failures::{global as llm_failures, LlmFailureKind};

/// Default embedding provider for configs that predate the
/// `embedding_provider` field. Kept as "openai" so existing installations
/// (whose config.toml has no such key) keep their current behavior; fresh
/// installs opt into local embeddings via the shipped config.toml.
fn default_embedding_provider() -> String {
    "openai".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// Completions provider (compression / reflect). Currently "openai".
    pub provider: String,
    /// Embedding provider: "local" (on-device ONNX) or "openai". Independent
    /// of `provider` so embeddings can run keyless while completions still use
    /// an API. Missing in older configs → defaults to "openai".
    #[serde(default = "default_embedding_provider")]
    pub embedding_provider: String,
    pub api_key: Option<String>,
    pub api_key_env: String,
    pub embedding_model: String,
    /// Directory holding the local ONNX model (`model.onnx` + `tokenizer.json`)
    /// when `embedding_provider = "local"`. Falls back to `embedding_model`
    /// (treated as a path) when unset.
    #[serde(default)]
    pub embedding_model_path: Option<String>,
    pub embedding_dim: usize,
    pub compression_model: String,
    pub timeout_secs: u64,
    pub fallback_to_regex: bool,
    /// Max idle keep-alive connections retained per host in the shared HTTP
    /// client pool. Bounds the pool so long-running instances don't accumulate
    /// stale sockets to `api.openai.com` over hours. `#[serde(default)]` keeps
    /// configs that predate the field loadable.
    #[serde(default = "default_pool_max_idle_per_host")]
    pub pool_max_idle_per_host: usize,
    /// How long an idle keep-alive connection may live before the pool recycles
    /// it (seconds). Without this, half-open/zombie connections survive for the
    /// life of the process and new requests hang or return zero-fact bodies
    /// (server-degradation hypothesis A). Actively recycling keeps the pool
    /// healthy.
    #[serde(default = "default_pool_idle_timeout_secs")]
    pub pool_idle_timeout_secs: u64,
    /// TCP keep-alive probe interval (seconds) for pooled connections, so dead
    /// peers are detected instead of lingering as zombies.
    #[serde(default = "default_tcp_keepalive_secs")]
    pub tcp_keepalive_secs: u64,
}

fn default_pool_max_idle_per_host() -> usize {
    16
}

fn default_pool_idle_timeout_secs() -> u64 {
    30
}

fn default_tcp_keepalive_secs() -> u64 {
    30
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: "openai".to_string(),
            embedding_provider: "openai".to_string(),
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            embedding_model: "text-embedding-3-small".to_string(),
            embedding_model_path: None,
            embedding_dim: 1536,
            compression_model: "gpt-4o-mini".to_string(),
            timeout_secs: 30,
            fallback_to_regex: true,
            pool_max_idle_per_host: default_pool_max_idle_per_host(),
            pool_idle_timeout_secs: default_pool_idle_timeout_secs(),
            tcp_keepalive_secs: default_tcp_keepalive_secs(),
        }
    }
}

impl LlmConfig {
    /// Build the shared HTTP client for all OpenAI-bound calls (extraction,
    /// embeddings, rerank, consolidation). Pool settings come from config so
    /// long-running deployments stay healthy: the idle pool is bounded and
    /// actively recycled instead of letting keep-alive sockets zombie over
    /// hours (server-degradation hypothesis A). One builder, one source of
    /// truth — both server and CLI re-embed paths go through here.
    pub fn build_http_client(&self) -> Result<Client> {
        Client::builder()
            .timeout(Duration::from_secs(self.timeout_secs))
            .pool_max_idle_per_host(self.pool_max_idle_per_host)
            .pool_idle_timeout(Duration::from_secs(self.pool_idle_timeout_secs))
            .tcp_keepalive(Duration::from_secs(self.tcp_keepalive_secs))
            .build()
            .context("Failed to build HTTP client")
    }

    pub fn get_api_key(&self) -> Option<String> {
        if let Some(ref key) = self.api_key {
            if !key.is_empty() {
                return Some(key.clone());
            }
        }
        std::env::var(&self.api_key_env).ok()
    }

    /// Per-instance env overrides for embeddings, so a cloud/Docker deployment
    /// can switch provider/dim without editing `config.toml` (local stays the
    /// keyless default). `LOOMEM_EMBEDDING_PROVIDER` (local|openai) and
    /// `LOOMEM_EMBEDDING_DIM` (positive integer). Unknown values are ignored
    /// with a WARN to avoid a silent regression on a typo.
    pub fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("LOOMEM_EMBEDDING_PROVIDER") {
            match v.as_str() {
                "local" | "openai" => self.embedding_provider = v,
                other => tracing::warn!(
                    "LOOMEM_EMBEDDING_PROVIDER={:?} not recognized (expected local/openai), keeping current value {}",
                    other,
                    self.embedding_provider
                ),
            }
        }
        if let Ok(v) = std::env::var("LOOMEM_EMBEDDING_DIM") {
            match v.parse::<usize>() {
                Ok(d) if d > 0 => self.embedding_dim = d,
                _ => tracing::warn!(
                    "LOOMEM_EMBEDDING_DIM={:?} not a positive integer, keeping current value {}",
                    v,
                    self.embedding_dim
                ),
            }
        }
    }
}

pub const PROMPT_VERSION: u32 = 3;

const COMPRESS_PROMPT_OBSERVATION: &str = "Extract observations from these memory fragments. Each observation is a single, self-contained fact, decision, preference, or event with its original context.\n\nRules:\n1. One observation per line — do NOT merge multiple facts into one sentence\n2. Preserve exact dates, names, numbers — do not generalize\n3. Annotate priority: [!] critical (decisions, preferences), [*] important (project details, deadlines), [.] info (context, background)\n4. If a memory references a change/update, include BOTH old and new state\n5. Language: match the original\n6. Maximum: 20 observations\n\nFormat — numbered list:\n1. [!] Example decision or preference\n2. [*] Example project detail\n3. [.] Example background info";

const COMPRESS_PROMPT_SUMMARY: &str = "Summarize these memory fragments into a single concise paragraph. Preserve: names, dates, decisions, facts, numbers. Drop: greetings, filler, repetition. Language: match the original. Max: 200 words.";

const COMPRESS_PROMPT_STRUCTURED: &str = r#"Extract observations from these memory fragments as structured JSON.

For each observation, classify its type:
- "event": Something that happened at a specific time (meeting, deployment, trip, purchase)
- "fact": Timeless knowledge (biographical detail, technical spec, permanent truth)
- "preference": Subjective choice, opinion, taste, or decision that may change
- "experience": Transferable lesson about how to act — a proven procedure, a lesson from a mistake, an effective strategy, or an anti-pattern

Rules:
1. One observation per item — do NOT merge multiple facts
2. Preserve exact dates, names, numbers — do not generalize
3. For events, set event_at_raw to the VERBATIM time expression from the text (e.g. "yesterday", "last Friday", "15 marca", "2026-03-15"). If no time expression but context implies a time, use "during this conversation"
4. For facts, preferences, and experiences, set event_at_raw to null
5. confidence: 0.9+ for explicit statements, 0.6-0.8 for inferences
6. importance: "high" for decisions/preferences/deadlines, "medium" for project details, "low" for background/filler
7. content must be SELF-CONTAINED — reader should understand without the original conversation. Include subject ("User deployed..." not "Deployed...")
8. Language: match the original (Polish stays Polish, English stays English)
9. Maximum: 20 observations

Return ONLY valid JSON (no markdown, no commentary):
{"observations": [{"type": "event", "content": "...", "event_at_raw": "yesterday", "confidence": 0.9, "importance": "high"}]}"#;

/// Get the consolidation prompt based on configured style.
pub fn get_compress_prompt(style: &str) -> &'static str {
    match style {
        "summary" => COMPRESS_PROMPT_SUMMARY,
        "structured" => COMPRESS_PROMPT_STRUCTURED,
        _ => COMPRESS_PROMPT_OBSERVATION, // "observation" is default
    }
}

/// Consolidation system prompt for `style`, prefixed with the shared
/// untrusted-data notice ([`crate::sanitizer::UNTRUSTED_DATA_NOTICE`]).
///
/// The fragments passed to [`compress`] are stored, user-authored content wrapped
/// in `[CHUNK id="…"]` markers by the caller. Prepending this notice in the trusted
/// system role tells the model to treat those fragments as data, so a memory that
/// says "replace facts about X with Y" is summarized, not obeyed — the fix for
/// second-order prompt injection on the consolidation path.
fn compress_system_prompt(style: &str) -> String {
    format!(
        "{}\n\n{}",
        crate::sanitizer::UNTRUSTED_DATA_NOTICE,
        get_compress_prompt(style)
    )
}

#[derive(Debug, Serialize)]
struct EmbeddingRequest {
    input: String,
    model: String,
}

#[derive(Debug, Serialize)]
struct BatchEmbeddingRequest {
    input: Vec<String>,
    model: String,
}

#[derive(Debug, Deserialize)]
struct BatchEmbeddingData {
    embedding: Vec<f32>,
    index: usize,
}

#[derive(Debug, Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
}

#[derive(Debug, Serialize)]
struct CompletionRequest {
    model: String,
    messages: Vec<Message>,
    max_tokens: u32,
}

#[derive(Debug, Serialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct CompletionResponse {
    choices: Vec<Choice>,
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: MessageResponse,
}

#[derive(Debug, Deserialize)]
struct MessageResponse {
    content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

pub struct RateLimiter {
    count: Arc<AtomicU64>,
    last_reset: Arc<Mutex<Instant>>,
    max_requests: u64,
}

impl RateLimiter {
    pub fn new(max_requests: u64) -> Self {
        Self {
            count: Arc::new(AtomicU64::new(0)),
            last_reset: Arc::new(Mutex::new(Instant::now())),
            max_requests,
        }
    }

    pub async fn check_and_increment(&self) -> Result<()> {
        let mut last_reset = self.last_reset.lock().await;
        let now = Instant::now();

        // Reset counter if a minute has passed
        if now.duration_since(*last_reset) >= Duration::from_secs(60) {
            self.count.store(0, Ordering::SeqCst);
            *last_reset = now;
        }

        let current = self.count.fetch_add(1, Ordering::SeqCst);
        if current >= self.max_requests {
            anyhow::bail!(
                "Rate limit exceeded: {} requests per minute",
                self.max_requests
            );
        }

        Ok(())
    }
}

/// Call OpenAI embedding API with retry logic
pub async fn embed(client: &Client, config: &LlmConfig, text: &str) -> Result<Vec<f32>> {
    let api_key = config
        .get_api_key()
        .context("OpenAI API key not configured")?;

    let url = "https://api.openai.com/v1/embeddings";

    let request_body = EmbeddingRequest {
        input: text.to_string(),
        model: config.embedding_model.clone(),
    };

    let timeout = Duration::from_secs(10);

    // Try with one retry on failure
    for attempt in 1..=2 {
        debug!(
            "Embedding attempt {} for text length {}",
            attempt,
            text.len()
        );

        let response = client
            .post(url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .json(&request_body)
            .timeout(timeout)
            .send()
            .await;

        match response {
            Ok(resp) => {
                let status = resp.status();

                // Retry on rate limit or server errors
                if (status == 429 || status.is_server_error()) && attempt < 2 {
                    warn!("Embedding API error (status {}), retrying...", status);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }

                if status.is_success() {
                    let body = resp
                        .json::<EmbeddingResponse>()
                        .await
                        .context("Failed to parse embedding response")?;

                    let embedding = body
                        .data
                        .into_iter()
                        .next()
                        .map(|d| d.embedding)
                        .context("No embedding data in response")?;

                    debug!(
                        "Successfully generated embedding with dim {}",
                        embedding.len()
                    );
                    return Ok(embedding);
                } else {
                    let error_text = resp
                        .text()
                        .await
                        .unwrap_or_else(|_| "unknown error".to_string());
                    // /157 S3: count the final failure for llm_failures_recent.
                    llm_failures().record(LlmFailureKind::Embedding);
                    anyhow::bail!(
                        "Embedding API failed with status {}: {}",
                        status,
                        error_text
                    );
                }
            }
            Err(e) => {
                warn!("Embedding request failed (attempt {}): {}", attempt, e);

                if attempt < 2 {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }

                llm_failures().record(LlmFailureKind::Embedding);
                return Err(e).context("Failed to send embedding request after retries");
            }
        }
    }

    anyhow::bail!("Embedding failed after all retries")
}

/// Batch embed multiple texts in a single API call. Returns embeddings
/// in the same order as input texts. OpenAI supports up to 2048 inputs.
pub async fn embed_batch(
    client: &Client,
    config: &LlmConfig,
    texts: &[String],
) -> Result<Vec<Vec<f32>>> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    if texts.len() == 1 {
        let emb = embed(client, config, &texts[0]).await?;
        return Ok(vec![emb]);
    }

    let api_key = config
        .get_api_key()
        .context("OpenAI API key not configured")?;

    let url = "https://api.openai.com/v1/embeddings";

    let request_body = BatchEmbeddingRequest {
        input: texts.to_vec(),
        model: config.embedding_model.clone(),
    };

    let timeout = Duration::from_secs(30); // longer timeout for batches

    for attempt in 1..=2 {
        debug!(
            "Batch embedding attempt {} for {} texts",
            attempt,
            texts.len()
        );

        let response = client
            .post(url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .json(&request_body)
            .timeout(timeout)
            .send()
            .await;

        match response {
            Ok(resp) => {
                let status = resp.status();

                if (status == 429 || status.is_server_error()) && attempt < 2 {
                    warn!("Batch embedding API error (status {}), retrying...", status);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }

                if status.is_success() {
                    #[derive(Debug, Deserialize)]
                    struct BatchResponse {
                        data: Vec<BatchEmbeddingData>,
                    }

                    let body = resp
                        .json::<BatchResponse>()
                        .await
                        .context("Failed to parse batch embedding response")?;

                    // Sort by index to match input order
                    let mut sorted = body.data;
                    sorted.sort_by_key(|d| d.index);

                    let embeddings: Vec<Vec<f32>> =
                        sorted.into_iter().map(|d| d.embedding).collect();

                    if embeddings.len() != texts.len() {
                        anyhow::bail!(
                            "Batch embedding returned {} results for {} inputs",
                            embeddings.len(),
                            texts.len()
                        );
                    }

                    debug!("Batch embedded {} texts successfully", embeddings.len());
                    return Ok(embeddings);
                } else {
                    let error_text = resp
                        .text()
                        .await
                        .unwrap_or_else(|_| "unknown error".to_string());
                    // /157 S3: count the final failure for llm_failures_recent.
                    llm_failures().record(LlmFailureKind::Embedding);
                    anyhow::bail!(
                        "Batch embedding API failed with status {}: {}",
                        status,
                        error_text
                    );
                }
            }
            Err(e) => {
                warn!(
                    "Batch embedding request failed (attempt {}): {}",
                    attempt, e
                );
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
                llm_failures().record(LlmFailureKind::Embedding);
                return Err(e).context("Failed to send batch embedding request after retries");
            }
        }
    }

    anyhow::bail!("Batch embedding failed after all retries")
}

/// Compress multiple texts using LLM with regex fallback
pub async fn compress(
    client: &Client,
    config: &LlmConfig,
    texts: &[String],
    consolidation_style: Option<&str>,
) -> Result<(String, Usage)> {
    let api_key = config
        .get_api_key()
        .context("OpenAI API key not configured")?;

    let url = "https://api.openai.com/v1/chat/completions";

    // Concatenate all texts
    let combined_text = texts.join("\n\n---\n\n");

    let request_body = CompletionRequest {
        model: config.compression_model.clone(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: compress_system_prompt(consolidation_style.unwrap_or("observation")),
            },
            Message {
                role: "user".to_string(),
                content: combined_text.clone(),
            },
        ],
        max_tokens: 500,
    };

    let timeout = Duration::from_secs(30);

    // Try with one retry on failure
    for attempt in 1..=2 {
        debug!(
            "Compression attempt {} for {} texts (total {} chars)",
            attempt,
            texts.len(),
            combined_text.len()
        );

        let response = client
            .post(url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .json(&request_body)
            .timeout(timeout)
            .send()
            .await;

        match response {
            Ok(resp) => {
                let status = resp.status();

                // Retry on rate limit or server errors
                if (status == 429 || status.is_server_error()) && attempt < 2 {
                    warn!("Compression API error (status {}), retrying...", status);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }

                if status.is_success() {
                    let body = resp
                        .json::<CompletionResponse>()
                        .await
                        .context("Failed to parse completion response")?;

                    let summary = body
                        .choices
                        .into_iter()
                        .next()
                        .map(|c| c.message.content)
                        .context("No completion data in response")?;

                    let usage = body.usage.unwrap_or(Usage {
                        prompt_tokens: 0,
                        completion_tokens: 0,
                    });

                    debug!(
                        "Successfully compressed to {} chars (tokens: {}+{})",
                        summary.len(),
                        usage.prompt_tokens,
                        usage.completion_tokens
                    );
                    return Ok((summary, usage));
                } else {
                    let error_text = resp
                        .text()
                        .await
                        .unwrap_or_else(|_| "unknown error".to_string());
                    warn!(
                        "Compression API failed with status {}: {}",
                        status, error_text
                    );
                    // /157 S3: final LLM failure — counted even when the
                    // regex fallback below softens the outcome.
                    llm_failures().record(LlmFailureKind::Consolidation);

                    if attempt >= 2 && config.fallback_to_regex {
                        warn!("Falling back to regex compression");
                        return Ok((
                            regex_compress(&combined_text),
                            Usage {
                                prompt_tokens: 0,
                                completion_tokens: 0,
                            },
                        ));
                    }

                    anyhow::bail!(
                        "Compression API failed with status {}: {}",
                        status,
                        error_text
                    );
                }
            }
            Err(e) => {
                warn!("Compression request failed (attempt {}): {}", attempt, e);

                if attempt < 2 {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                }

                llm_failures().record(LlmFailureKind::Consolidation);
                if config.fallback_to_regex {
                    warn!("Falling back to regex compression after error: {}", e);
                    return Ok((
                        regex_compress(&combined_text),
                        Usage {
                            prompt_tokens: 0,
                            completion_tokens: 0,
                        },
                    ));
                }

                return Err(e).context("Failed to send compression request after retries");
            }
        }
    }

    anyhow::bail!("Compression failed after all retries")
}

const REFLECT_PROMPT: &str = "You are a memory quality auditor. Review these consolidated memory chunks and produce a JSON response with:
- \"contradictions\": array of {\"chunk_ids\": [id1, id2], \"description\": \"what conflicts\"}
- \"outdated\": array of {\"chunk_id\": id, \"reason\": \"why it seems outdated\"}
- \"gaps\": array of strings describing missing context or information holes
- \"quality_score\": float 0.0-1.0 (overall quality assessment)
- \"summary\": one sentence overall assessment
Respond ONLY with valid JSON, no markdown.";

/// Review consolidated (L1+) memories for contradictions, staleness, and gaps.
pub async fn reflect(
    client: &Client,
    config: &LlmConfig,
    chunks: &[(String, String)], // (id, content)
) -> Result<(serde_json::Value, Usage)> {
    let api_key = config
        .get_api_key()
        .context("OpenAI API key not configured")?;

    let combined: String = chunks
        .iter()
        .map(|(id, content)| format!("[CHUNK id=\"{}\"]\n{}\n[/CHUNK]", id, content))
        .collect::<Vec<_>>()
        .join("\n\n");

    let request_body = CompletionRequest {
        model: config.compression_model.clone(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: REFLECT_PROMPT.to_string(),
            },
            Message {
                role: "user".to_string(),
                content: combined,
            },
        ],
        max_tokens: 2000,
    };

    let resp = client
        .post("https://api.openai.com/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&request_body)
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .context("Reflect API request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("Reflect API error {}: {}", status, text);
    }

    let body: CompletionResponse = resp
        .json()
        .await
        .context("Failed to parse reflect response")?;

    let content = body
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .context("No reflect response")?;

    let usage = body.usage.unwrap_or(Usage {
        prompt_tokens: 0,
        completion_tokens: 0,
    });

    let result: serde_json::Value = serde_json::from_str(&content).unwrap_or_else(|_| {
        serde_json::json!({
            "raw_response": content,
            "parse_error": "LLM response was not valid JSON"
        })
    });

    Ok((result, usage))
}

/// Fallback regex-based compression: first 3 sentences + "..." + last sentence
fn regex_compress(text: &str) -> String {
    // This fallback output is stored verbatim, so strip any [CHUNK] boundary
    // markers first: they are LLM-only scaffolding (added by
    // `sanitizer::wrap_untrusted` for the compression prompt) and must never
    // land in a consolidated memory (sec/prompt-injection-delimiters, Greptile).
    let text = crate::sanitizer::strip_untrusted_markers(text);
    let sentences: Vec<&str> = text
        .split(['.', '!', '?'])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();

    if sentences.len() <= 4 {
        return text.to_string();
    }

    let first_three = sentences
        .iter()
        .take(3)
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(". ");
    let last_one = sentences.last().map(|s| s.to_string()).unwrap_or_default();

    format!("{}. ... {}", first_three, last_one)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared client builds from config pool settings without panicking,
    /// for both custom and defaulted values (server-degradation fix A1).
    #[test]
    fn build_http_client_succeeds_with_pool_settings() {
        let mut config = LlmConfig::default();
        config.pool_max_idle_per_host = 8;
        config.pool_idle_timeout_secs = 45;
        config.tcp_keepalive_secs = 15;
        assert!(config.build_http_client().is_ok());
        // Defaults are also valid (configs that predate the pool fields).
        assert!(LlmConfig::default().build_http_client().is_ok());
    }

    /// Every consolidation style carries the untrusted-data notice ahead of its
    /// style-specific instructions, so wrapped memory fragments are treated as
    /// data, not instructions (second-order prompt-injection defense).
    #[test]
    fn compress_system_prompt_prepends_untrusted_notice() {
        for style in ["observation", "summary", "structured", "unknown"] {
            let prompt = compress_system_prompt(style);
            assert!(
                prompt.starts_with(crate::sanitizer::UNTRUSTED_DATA_NOTICE),
                "style {style} missing untrusted-data notice"
            );
            assert!(
                prompt.ends_with(get_compress_prompt(style)),
                "style {style} dropped its base prompt"
            );
        }
    }

    #[test]
    fn test_regex_fallback() {
        let text =
            "First sentence. Second sentence. Third sentence. Fourth sentence. Fifth sentence.";
        let compressed = regex_compress(text);

        assert!(compressed.contains("First sentence"));
        assert!(compressed.contains("Second sentence"));
        assert!(compressed.contains("Third sentence"));
        assert!(compressed.contains("..."));
        assert!(compressed.contains("Fifth sentence"));
        assert!(!compressed.contains("Fourth sentence"));
    }

    #[test]
    fn test_regex_fallback_short_text() {
        let text = "First sentence. Second sentence.";
        let compressed = regex_compress(text);

        assert_eq!(compressed, text);
    }

    #[test]
    fn regex_fallback_strips_chunk_markers() {
        // The fallback output is stored verbatim, so [CHUNK] prompt scaffolding
        // must not survive — even on the short-text early-return path.
        let wrapped =
            crate::sanitizer::wrap_untrusted("L0:a", "one sentence. two sentence. three.");
        let compressed = regex_compress(&wrapped);
        assert!(!compressed.contains("[CHUNK"));
        assert!(!compressed.contains("[/CHUNK]"));
        assert!(compressed.contains("one sentence"));
    }

    #[tokio::test]
    async fn test_rate_limiter() {
        let limiter = RateLimiter::new(2);

        assert!(limiter.check_and_increment().await.is_ok());
        assert!(limiter.check_and_increment().await.is_ok());
        assert!(limiter.check_and_increment().await.is_err());
    }

    // Env-var tests mutate process-global state and race the multi-threaded
    // cargo test runner; `serial_test` is not a dependency (CLAUDE.md §7), so
    // they are #[ignore]d and run manually — mirrors `access_audit::config::tests`.
    //   cargo test -p loomem-core --lib -- --ignored --test-threads=1 apply_env

    #[test]
    #[ignore = "env-var race; manually verified (serial_test not in deps)"]
    fn env_overrides_embedding_provider_and_dim() {
        let mut cfg = LlmConfig::default();
        cfg.embedding_provider = "local".to_string();
        cfg.embedding_dim = 384;
        std::env::set_var("LOOMEM_EMBEDDING_PROVIDER", "openai");
        std::env::set_var("LOOMEM_EMBEDDING_DIM", "1536");
        cfg.apply_env_overrides();
        std::env::remove_var("LOOMEM_EMBEDDING_PROVIDER");
        std::env::remove_var("LOOMEM_EMBEDDING_DIM");
        assert_eq!(cfg.embedding_provider, "openai");
        assert_eq!(cfg.embedding_dim, 1536);
    }

    #[test]
    #[ignore = "env-var race; manually verified (serial_test not in deps)"]
    fn env_unknown_embedding_values_keep_current() {
        let mut cfg = LlmConfig::default();
        cfg.embedding_provider = "local".to_string();
        cfg.embedding_dim = 384;
        std::env::set_var("LOOMEM_EMBEDDING_PROVIDER", "bogus");
        std::env::set_var("LOOMEM_EMBEDDING_DIM", "-1");
        cfg.apply_env_overrides();
        std::env::remove_var("LOOMEM_EMBEDDING_PROVIDER");
        std::env::remove_var("LOOMEM_EMBEDDING_DIM");
        assert_eq!(
            cfg.embedding_provider, "local",
            "unknown provider must not change value"
        );
        assert_eq!(cfg.embedding_dim, 384, "invalid dim must not change value");
    }
}
