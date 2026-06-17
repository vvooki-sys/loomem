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
        }
    }
}

impl LlmConfig {
    pub fn get_api_key(&self) -> Option<String> {
        if let Some(ref key) = self.api_key {
            if !key.is_empty() {
                return Some(key.clone());
            }
        }
        std::env::var(&self.api_key_env).ok()
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
                content: get_compress_prompt(consolidation_style.unwrap_or("observation"))
                    .to_string(),
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

    #[tokio::test]
    async fn test_rate_limiter() {
        let limiter = RateLimiter::new(2);

        assert!(limiter.check_and_increment().await.is_ok());
        assert!(limiter.check_and_increment().await.is_ok());
        assert!(limiter.check_and_increment().await.is_err());
    }
}
