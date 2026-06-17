//! Knowledge extraction pipeline.
//!
//! Extracts typed, temporally-annotated facts from conversation text using LLM.
//! Uses 4-type taxonomy: preference_or_decision, project_state, fact, experience.

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{debug, warn};

use crate::config::LlmConfig;

/// /153: an operator-defined extraction category. `fact_type` is the wire key
/// used in the `## Types` list and the JSON `fact_type` enum; when absent,
/// `name` is used as the key. `description` is the LLM-facing definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionTopic {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub fact_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeExtractionConfig {
    pub enabled: bool,
    pub model: String,
    pub max_transcript_tokens: usize,
    pub dedup_cosine_threshold: f64,
    pub contradiction_check: bool,
    pub contradiction_cosine_min: f64,
    pub max_facts_per_transcript: usize,
    /// /157 S1 (AC-7): completion cap for each extraction request. Was a
    /// hardcoded `2000` in the request body; serde default keeps a config
    /// without this field producing a byte-identical request.
    #[serde(default = "default_extraction_max_tokens")]
    pub max_tokens: u32,
    /// /153: operator-defined extraction categories. `None`/empty preserves
    /// the built-in [`EXTRACTION_PROMPT`] byte-for-byte (serde default keeps
    /// a config without this field deserializing unchanged).
    #[serde(default)]
    pub topics: Option<Vec<ExtractionTopic>>,
}

fn default_extraction_max_tokens() -> u32 {
    2000
}

impl Default for KnowledgeExtractionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: "gpt-4.1-mini".to_string(),
            max_transcript_tokens: 20000,
            dedup_cosine_threshold: 0.92,
            contradiction_check: true,
            contradiction_cosine_min: 0.5,
            max_facts_per_transcript: 20,
            max_tokens: default_extraction_max_tokens(),
            topics: None,
        }
    }
}

const EXTRACTION_PROMPT: &str = r#"Extract factual knowledge from this conversation. Each fact is a self-contained sentence.

## Types (exactly one per fact):
- "preference_or_decision": Choices, preferences, decisions, opinions. E.g. "Anna prefers dark mode", "Team decided to use Rust for the backend".
- "project_state": Current state of work, ongoing projects, deadlines, statuses. E.g. "Auth migration is blocked by legal review", "Sprint ends on 2026-03-15".
- "fact": Biographical, permanent facts. E.g. "Anna is a senior engineer at Acme", "The main repo is github.com/acme/core".
- "experience": Transferable lesson about how to act — proven procedure, lesson from a mistake, effective strategy, anti-pattern. E.g. "When dispatching long tasks to Claude Code, the full brief must be in the first message — drip-feeding instructions degrades output quality", "Running cargo clippy before cargo test catches most issues earlier and saves CI time".

## Rules:
1. Each fact must be a single, self-contained sentence in natural language
2. Extract: preferences, decisions, facts about people, project states, deadlines, contacts, technical decisions, procedural lessons
3. SKIP: greetings, small talk, questions without answers, uncertain statements, action items, temporary debugging states, summaries
4. CHANGE RULE: When something changed, always record before→after. E.g. "Anna switched from VS Code to Neovim" (not just "Anna uses Neovim")
5. THREE DATES MODEL: For each fact, provide:
   - "event_date": absolute ISO date if determinable (e.g. "2026-03-15"), null otherwise
   - "event_date_context": original relative expression if any (e.g. "yesterday", "two weeks ago"), null otherwise
6. "subject": The main entity this fact is about (person name, project name, etc.), null if unclear
7. Confidence: 0.9+ for explicit statements, 0.6-0.8 for inferred, skip below 0.5
8. Preserve original language and diacritics
9. Maximum 20 facts per conversation

The conversation_date is: {conversation_date}

Return ONLY valid JSON (no markdown, no code blocks):
{"facts": [{"content": "...", "fact_type": "preference_or_decision"|"project_state"|"fact"|"experience", "subject": "...", "event_date": "2026-03-15"|null, "event_date_context": "yesterday"|null, "confidence": 0.9}]}"#;

/// /153: build the extraction system prompt for one request.
///
/// With no (or empty) `config.topics` the result is **byte-identical** to
/// [`EXTRACTION_PROMPT`] with `{conversation_date}` substituted — the pre-/153
/// behavior (AC-2). With operator-defined topics, the `## Types` list and the
/// JSON `fact_type` enum are rebuilt from them (each topic's `fact_type` field,
/// or `name` as the wire key); every other prompt section is preserved.
pub fn build_extraction_prompt(
    config: &KnowledgeExtractionConfig,
    conversation_date: &str,
) -> String {
    let topics = match config.topics.as_ref() {
        Some(t) if !t.is_empty() => t,
        _ => return EXTRACTION_PROMPT.replace("{conversation_date}", conversation_date),
    };

    let type_lines = topics
        .iter()
        .map(|t| {
            let key = t.fact_type.as_deref().unwrap_or(t.name.as_str());
            format!("- \"{key}\": {}", t.description)
        })
        .collect::<Vec<_>>()
        .join("\n");

    let enum_values = topics
        .iter()
        .map(|t| {
            let key = t.fact_type.as_deref().unwrap_or(t.name.as_str());
            format!("\"{key}\"")
        })
        .collect::<Vec<_>>()
        .join("|");

    format!(
        r#"Extract factual knowledge from this conversation. Each fact is a self-contained sentence.

## Types (exactly one per fact):
{type_lines}

## Rules:
1. Each fact must be a single, self-contained sentence in natural language
2. Extract: preferences, decisions, facts about people, project states, deadlines, contacts, technical decisions, procedural lessons
3. SKIP: greetings, small talk, questions without answers, uncertain statements, action items, temporary debugging states, summaries
4. CHANGE RULE: When something changed, always record before→after. E.g. "Anna switched from VS Code to Neovim" (not just "Anna uses Neovim")
5. THREE DATES MODEL: For each fact, provide:
   - "event_date": absolute ISO date if determinable (e.g. "2026-03-15"), null otherwise
   - "event_date_context": original relative expression if any (e.g. "yesterday", "two weeks ago"), null otherwise
6. "subject": The main entity this fact is about (person name, project name, etc.), null if unclear
7. Confidence: 0.9+ for explicit statements, 0.6-0.8 for inferred, skip below 0.5
8. Preserve original language and diacritics
9. Maximum 20 facts per conversation

The conversation_date is: {conversation_date}

Return ONLY valid JSON (no markdown, no code blocks):
{{"facts": [{{"content": "...", "fact_type": {enum_values}, "subject": "...", "event_date": "2026-03-15"|null, "event_date_context": "yesterday"|null, "confidence": 0.9}}]}}"#,
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedFact {
    pub content: String,
    pub fact_type: String,
    pub subject: Option<String>,
    pub event_date: Option<String>,
    pub event_date_context: Option<String>,
    pub confidence: f64,
}

/// Legacy alias for backward compatibility with ingest_conversation_handler
pub type ExtractedMemory = ExtractedFact;

impl ExtractedFact {
    /// Map fact_type string to storage::FactType
    pub fn to_fact_type(&self) -> crate::storage::FactType {
        match self.fact_type.as_str() {
            "preference_or_decision" => crate::storage::FactType::PreferenceOrDecision,
            "project_state" => crate::storage::FactType::ProjectState,
            "experience" => crate::storage::FactType::Experience,
            _ => crate::storage::FactType::Fact,
        }
    }

    /// Build ExtractionMeta from this extracted fact
    pub fn to_extraction_meta(
        &self,
        extracted_from: Option<String>,
        model: &str,
    ) -> crate::storage::ExtractionMeta {
        crate::storage::ExtractionMeta {
            fact_type: self.to_fact_type(),
            subject: self.subject.clone(),
            event_date: self.event_date.clone(),
            event_date_context: self.event_date_context.clone(),
            supersedes: None,
            superseded_by: None,
            confidence: self.confidence,
            extracted_from,
            extraction_model: Some(model.to_string()),
            original_content: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionResult {
    pub facts: Vec<ExtractedFact>,
}

/// Max chars kept of an error body / parse message in a [`ChunkFailure`].
const MAX_FAILURE_REASON_CHARS: usize = 200;

/// Truncate an error body / parse message to a digest-sized reason string.
fn truncate_reason(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_FAILURE_REASON_CHARS {
        trimmed.to_string()
    } else {
        let cut: String = trimmed.chars().take(MAX_FAILURE_REASON_CHARS).collect();
        format!("{cut}…")
    }
}

/// Minimal HTTP-shaped reply the extraction loop needs from the chat API.
#[derive(Debug, Clone)]
pub struct ChatHttpReply {
    /// HTTP status code of the chat-completion response.
    pub status: u16,
    /// Raw response body text (success envelope or error JSON).
    pub body: String,
}

/// Seam between the extraction loop and the OpenAI HTTP call (/157 S1).
/// Production impl is [`HttpExtractionChat`]; tests inject failing stubs.
/// Mirrors `ContentTypeClassifier` (ADR-014 trait-DI, native AFIT).
pub trait ExtractionChat: Send + Sync {
    /// Send one chat-completion request body. `Err` = transport-level
    /// failure (no HTTP response at all).
    fn chat(
        &self,
        request_body: &serde_json::Value,
    ) -> impl std::future::Future<Output = Result<ChatHttpReply>> + Send;
}

/// Production [`ExtractionChat`]: posts to the OpenAI chat endpoint.
pub struct HttpExtractionChat<'a> {
    client: &'a Client,
    api_key: String,
}

impl<'a> HttpExtractionChat<'a> {
    pub fn new(client: &'a Client, api_key: String) -> Self {
        Self { client, api_key }
    }
}

impl ExtractionChat for HttpExtractionChat<'_> {
    fn chat(
        &self,
        request_body: &serde_json::Value,
    ) -> impl std::future::Future<Output = Result<ChatHttpReply>> + Send {
        // Clone captures so the future owns them (Send, no borrow across await).
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let body = request_body.clone();
        async move {
            let response = client
                .post("https://api.openai.com/v1/chat/completions")
                .header("Authorization", format!("Bearer {}", api_key))
                .header("Content-Type", "application/json")
                .json(&body)
                .timeout(Duration::from_secs(30))
                .send()
                .await
                .context("Failed to send extraction request")?;
            let status = response.status().as_u16();
            let body = response
                .text()
                .await
                .context("Failed to read extraction response body")?;
            Ok(ChatHttpReply { status, body })
        }
    }
}

/// One failed extraction attempt for a single transcript chunk (/157 S1).
#[derive(Debug, Clone)]
pub struct ChunkFailure {
    /// 0-based index of the transcript chunk that failed.
    pub chunk_index: usize,
    /// HTTP status when the API answered non-2xx (or a 2xx body failed to
    /// parse); `None` for transport-level failures.
    pub status: Option<u16>,
    /// Truncated digest of the error body / parse error.
    pub reason: String,
}

impl ChunkFailure {
    /// Label for the user-facing `Extraction failed (<status>)` message:
    /// the HTTP status when the API answered, `"network"` otherwise.
    pub fn status_label(&self) -> String {
        match self.status {
            Some(s) => s.to_string(),
            None => "network".to_string(),
        }
    }
}

fn first_status_label(failures: &[ChunkFailure]) -> String {
    failures
        .first()
        .map(ChunkFailure::status_label)
        .unwrap_or_else(|| "unknown".to_string())
}

fn first_reason(failures: &[ChunkFailure]) -> &str {
    failures.first().map_or("unknown", |f| f.reason.as_str())
}

/// Extraction produced no usable result (/157 S1): either the API key is
/// missing or every transcript chunk failed. Partial failures (some chunks
/// succeeded) are NOT an error; they surface as [`ExtractionOutcome::failures`].
#[derive(Debug, thiserror::Error)]
pub enum ExtractionError {
    #[error("OpenAI API key not configured for knowledge extraction")]
    NoApiKey,
    #[error(
        "knowledge extraction failed: {}/{} chunk(s) failed; first: ({}) {}",
        .failures.len(),
        .chunks_attempted,
        first_status_label(.failures),
        first_reason(.failures)
    )]
    AllChunksFailed {
        chunks_attempted: usize,
        failures: Vec<ChunkFailure>,
    },
}

impl ExtractionError {
    /// `(status_label, reason)` pair for the user-facing
    /// `Extraction failed (<status>): <reason>` message.
    pub fn status_and_reason(&self) -> (String, String) {
        match self {
            Self::NoApiKey => (
                "config".to_string(),
                "OpenAI API key not configured".to_string(),
            ),
            Self::AllChunksFailed { failures, .. } => (
                first_status_label(failures),
                first_reason(failures).to_string(),
            ),
        }
    }
}

/// Result of an (at least partially) successful extraction run (/157 S1).
#[derive(Debug, Default)]
pub struct ExtractionOutcome {
    pub facts: Vec<ExtractedFact>,
    /// Per-chunk failures when other chunks still succeeded (partial
    /// success). Empty on full success; all-chunks-failed is
    /// [`ExtractionError::AllChunksFailed`] instead.
    pub failures: Vec<ChunkFailure>,
}

/// Build the chat-completion request body for one transcript chunk.
/// `max_tokens` comes from [`KnowledgeExtractionConfig::max_tokens`] (AC-7).
fn build_extraction_request(
    model: &str,
    system_prompt: &str,
    chunk_text: &str,
    max_tokens: u32,
) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": chunk_text}
        ],
        "max_tokens": max_tokens,
        "temperature": 0.0
    })
}

/// Parse a 2xx chat reply body into facts (LLM envelope + extraction JSON).
fn parse_extraction_reply(
    reply: &ChatHttpReply,
    chunk_index: usize,
) -> Result<Vec<ExtractedFact>, ChunkFailure> {
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

    let llm_resp: LlmResponse = serde_json::from_str(&reply.body).map_err(|e| ChunkFailure {
        chunk_index,
        status: Some(reply.status),
        reason: truncate_reason(&format!("response parse failed: {e}")),
    })?;

    let content = llm_resp
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

    serde_json::from_str::<ExtractionResult>(json_str)
        .map(|result| result.facts)
        .map_err(|e| ChunkFailure {
            chunk_index,
            status: Some(reply.status),
            reason: truncate_reason(&format!("facts parse failed: {e}")),
        })
}

/// Run one chunk's request → parse pipeline. `Err` carries the per-chunk
/// failure (HTTP non-2xx, transport, or parse) for aggregation (/157 S1).
async fn extract_chunk(
    chat: &impl ExtractionChat,
    request_body: &serde_json::Value,
    chunk_index: usize,
) -> Result<Vec<ExtractedFact>, ChunkFailure> {
    let reply = match chat.chat(request_body).await {
        Ok(r) => r,
        Err(e) => {
            return Err(ChunkFailure {
                chunk_index,
                status: None,
                reason: truncate_reason(&format!("{e:#}")),
            })
        }
    };

    if !(200..300).contains(&reply.status) {
        return Err(ChunkFailure {
            chunk_index,
            status: Some(reply.status),
            reason: truncate_reason(&reply.body),
        });
    }

    parse_extraction_reply(&reply, chunk_index)
}

/// Extract knowledge facts from conversation text using LLM.
///
/// `conversation_date` should be ISO format (e.g. "2026-04-01") for resolving relative dates.
///
/// /157 S1 error contract:
/// - `Ok` with non-empty `facts` — success (`failures` lists failed chunks),
/// - `Ok` with empty `facts` and empty `failures` — genuine zero facts,
/// - `Err` — no API key, or every chunk failed (HTTP / transport / parse).
pub async fn extract_knowledge(
    client: &Client,
    llm_config: &LlmConfig,
    extraction_config: &KnowledgeExtractionConfig,
    conversation_text: &str,
    conversation_date: &str,
) -> Result<ExtractionOutcome, ExtractionError> {
    let api_key = llm_config.get_api_key().ok_or(ExtractionError::NoApiKey)?;
    let chat = HttpExtractionChat::new(client, api_key);
    extract_knowledge_with(
        &chat,
        extraction_config,
        conversation_text,
        conversation_date,
    )
    .await
}

/// Extraction loop with an injected chat transport (/157 S1) — the seam the
/// dispatcher integration test uses to simulate API failures (AC-1).
pub async fn extract_knowledge_with(
    chat: &impl ExtractionChat,
    extraction_config: &KnowledgeExtractionConfig,
    conversation_text: &str,
    conversation_date: &str,
) -> Result<ExtractionOutcome, ExtractionError> {
    let system_prompt = build_extraction_prompt(extraction_config, conversation_date);

    // Split into chunks if too long (rough estimate: 4 chars ≈ 1 token)
    let max_chars = extraction_config.max_transcript_tokens * 4;
    let chunks = if conversation_text.len() > max_chars {
        split_with_overlap(conversation_text, max_chars, max_chars / 10)
    } else {
        vec![conversation_text.to_string()]
    };

    let mut outcome = ExtractionOutcome::default();
    let mut succeeded_chunks = 0usize;

    for (i, chunk_text) in chunks.iter().enumerate() {
        debug!(
            "Extracting knowledge from chunk {}/{} ({} chars)",
            i + 1,
            chunks.len(),
            chunk_text.len()
        );

        let request_body = build_extraction_request(
            &extraction_config.model,
            &system_prompt,
            chunk_text,
            extraction_config.max_tokens,
        );

        match extract_chunk(chat, &request_body, i).await {
            Ok(facts) => {
                succeeded_chunks += 1;
                let before = outcome.facts.len();
                outcome
                    .facts
                    .extend(facts.into_iter().filter(|f| f.confidence >= 0.5));
                debug!(
                    "Extracted {} facts (filtered by confidence >= 0.5)",
                    outcome.facts.len() - before
                );
            }
            Err(failure) => {
                warn!(
                    "Extraction LLM call failed for chunk {}/{} ({}): {}",
                    i + 1,
                    chunks.len(),
                    failure.status_label(),
                    failure.reason
                );
                // /157 S3: counted for llm_failures_recent in status payloads.
                crate::llm_failures::global()
                    .record(crate::llm_failures::LlmFailureKind::Extraction);
                outcome.failures.push(failure);
            }
        }

        // Respect extraction cap
        if outcome.facts.len() >= extraction_config.max_facts_per_transcript {
            outcome
                .facts
                .truncate(extraction_config.max_facts_per_transcript);
            break;
        }
    }

    if succeeded_chunks == 0 && !outcome.failures.is_empty() {
        return Err(ExtractionError::AllChunksFailed {
            chunks_attempted: chunks.len(),
            failures: outcome.failures,
        });
    }

    Ok(outcome)
}

/// Legacy wrapper — calls extract_knowledge with today's date.
pub async fn extract_memories(
    client: &Client,
    llm_config: &LlmConfig,
    extraction_config: &KnowledgeExtractionConfig,
    conversation_text: &str,
) -> Result<ExtractionOutcome, ExtractionError> {
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    extract_knowledge(
        client,
        llm_config,
        extraction_config,
        conversation_text,
        &today,
    )
    .await
}

/// Split text into overlapping chunks for LLM processing.
fn split_with_overlap(text: &str, chunk_size: usize, overlap: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut start = 0;

    while start < chars.len() {
        let end = (start + chunk_size).min(chars.len());
        chunks.push(chars[start..end].iter().collect());
        if end >= chars.len() {
            break;
        }
        start = end - overlap;
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Stub transport: always returns the same (status, body) reply.
    struct FixedReply {
        status: u16,
        body: String,
    }

    impl ExtractionChat for FixedReply {
        async fn chat(&self, _request_body: &serde_json::Value) -> Result<ChatHttpReply> {
            Ok(ChatHttpReply {
                status: self.status,
                body: self.body.clone(),
            })
        }
    }

    /// Stub transport: connection-level failure on every call.
    struct TransportFail;

    impl ExtractionChat for TransportFail {
        async fn chat(&self, _request_body: &serde_json::Value) -> Result<ChatHttpReply> {
            Err(anyhow::anyhow!("connection refused"))
        }
    }

    /// Stub transport: pops one scripted reply per call (multi-chunk tests).
    struct SequencedReplies(Mutex<Vec<Result<ChatHttpReply>>>);

    impl ExtractionChat for SequencedReplies {
        async fn chat(&self, _request_body: &serde_json::Value) -> Result<ChatHttpReply> {
            self.0.lock().expect("stub lock poisoned").remove(0)
        }
    }

    fn cfg() -> KnowledgeExtractionConfig {
        KnowledgeExtractionConfig::default()
    }

    /// Wrap an extraction-JSON string in the OpenAI chat envelope.
    fn envelope(facts_json: &str) -> String {
        serde_json::json!({
            "choices": [{"message": {"content": facts_json}}]
        })
        .to_string()
    }

    /// AC-1 (unit): HTTP 429 on the only chunk → loud `ExtractionError`
    /// carrying the status and a digest of the body — never `Ok(empty)`.
    #[tokio::test]
    async fn http_429_returns_error_not_empty_ok() {
        let chat = FixedReply {
            status: 429,
            body: r#"{"error":{"code":"insufficient_quota"}}"#.to_string(),
        };
        let err = extract_knowledge_with(&chat, &cfg(), "transcript", "2026-06-11")
            .await
            .expect_err("429 must surface as an error");
        let (status, reason) = err.status_and_reason();
        assert_eq!(status, "429");
        assert!(reason.contains("insufficient_quota"));
        match err {
            ExtractionError::AllChunksFailed {
                chunks_attempted,
                ref failures,
            } => {
                assert_eq!(chunks_attempted, 1);
                assert_eq!(failures.len(), 1);
                assert_eq!(failures[0].status, Some(429));
            }
            ExtractionError::NoApiKey => panic!("wrong variant: NoApiKey"),
        }
    }

    /// AC-2: a successful LLM reply with zero facts is Ok-and-empty with no
    /// failures — error-vs-empty distinction works in both directions.
    #[tokio::test]
    async fn success_with_zero_facts_is_ok_empty() {
        let chat = FixedReply {
            status: 200,
            body: envelope(r#"{"facts": []}"#),
        };
        let outcome = extract_knowledge_with(&chat, &cfg(), "transcript", "2026-06-11")
            .await
            .expect("zero facts is success, not an error");
        assert!(outcome.facts.is_empty());
        assert!(outcome.failures.is_empty());
    }

    /// Facts below the 0.5 confidence floor are filtered (pre-/157 behavior).
    #[tokio::test]
    async fn success_filters_low_confidence_facts() {
        let facts = r#"{"facts": [
            {"content": "A", "fact_type": "fact", "subject": null, "event_date": null, "event_date_context": null, "confidence": 0.9},
            {"content": "B", "fact_type": "fact", "subject": null, "event_date": null, "event_date_context": null, "confidence": 0.4}
        ]}"#;
        let chat = FixedReply {
            status: 200,
            body: envelope(facts),
        };
        let outcome = extract_knowledge_with(&chat, &cfg(), "transcript", "2026-06-11")
            .await
            .expect("success");
        assert_eq!(outcome.facts.len(), 1);
        assert_eq!(outcome.facts[0].content, "A");
        assert!(outcome.failures.is_empty());
    }

    /// Transport failure (no HTTP response) → "network" status label.
    #[tokio::test]
    async fn transport_failure_is_network_error() {
        let err = extract_knowledge_with(&TransportFail, &cfg(), "t", "2026-06-11")
            .await
            .expect_err("transport failure must surface as an error");
        let (status, reason) = err.status_and_reason();
        assert_eq!(status, "network");
        assert!(reason.contains("connection refused"));
    }

    /// Unparseable 2xx body → error mentioning the parse failure, not Ok(empty).
    #[tokio::test]
    async fn malformed_response_body_is_error() {
        let chat = FixedReply {
            status: 200,
            body: "not json".to_string(),
        };
        let err = extract_knowledge_with(&chat, &cfg(), "transcript", "2026-06-11")
            .await
            .expect_err("malformed body must surface as an error");
        let (status, reason) = err.status_and_reason();
        assert_eq!(status, "200");
        assert!(reason.contains("response parse failed"));
    }

    /// Multi-chunk partial success: the failing chunk is aggregated into
    /// `failures`, the surviving chunk's facts are returned (Ok, not Err).
    #[tokio::test]
    async fn partial_success_aggregates_failures() {
        let mut config = cfg();
        config.max_transcript_tokens = 5; // 20 chars per chunk → forces a split
        let text = "x".repeat(30); // → exactly 2 chunks (0..20, 18..30)
        let ok_body = envelope(
            r#"{"facts": [{"content": "kept", "fact_type": "fact", "subject": null, "event_date": null, "event_date_context": null, "confidence": 0.8}]}"#,
        );
        let chat = SequencedReplies(Mutex::new(vec![
            Ok(ChatHttpReply {
                status: 500,
                body: "boom".to_string(),
            }),
            Ok(ChatHttpReply {
                status: 200,
                body: ok_body,
            }),
        ]));
        let outcome = extract_knowledge_with(&chat, &config, &text, "2026-06-11")
            .await
            .expect("partial success is Ok");
        assert_eq!(outcome.facts.len(), 1);
        assert_eq!(outcome.facts[0].content, "kept");
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(outcome.failures[0].status, Some(500));
    }

    /// AC-7: default config produces a request byte-identical to the old
    /// hardcoded body (`max_tokens: 2000`).
    #[test]
    fn request_body_max_tokens_default_byte_identical() {
        let body = build_extraction_request("gpt-4.1-mini", "sys", "chunk", cfg().max_tokens);
        let legacy = serde_json::json!({
            "model": "gpt-4.1-mini",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "chunk"}
            ],
            "max_tokens": 2000,
            "temperature": 0.0
        });
        assert_eq!(body, legacy);
        assert_eq!(
            serde_json::to_string(&body).expect("serialize new body"),
            serde_json::to_string(&legacy).expect("serialize legacy body"),
        );
    }

    /// AC-7: config without the field deserializes to the 2000 default;
    /// an explicit value is honored.
    #[test]
    fn max_tokens_serde_default_and_override() {
        let base = serde_json::json!({
            "enabled": true,
            "model": "gpt-4.1-mini",
            "max_transcript_tokens": 20000,
            "dedup_cosine_threshold": 0.92,
            "contradiction_check": true,
            "contradiction_cosine_min": 0.5,
            "max_facts_per_transcript": 20
        });
        let without: KnowledgeExtractionConfig =
            serde_json::from_value(base.clone()).expect("config without max_tokens parses");
        assert_eq!(without.max_tokens, 2000);

        let mut with_field = base;
        with_field["max_tokens"] = serde_json::json!(512);
        let with: KnowledgeExtractionConfig =
            serde_json::from_value(with_field).expect("config with max_tokens parses");
        assert_eq!(with.max_tokens, 512);
    }

    /// Failure reasons are digest-sized: long bodies truncate with an ellipsis.
    #[test]
    fn reason_truncates_long_bodies() {
        let long = "x".repeat(500);
        let t = truncate_reason(&long);
        assert!(t.chars().count() <= MAX_FAILURE_REASON_CHARS + 1);
        assert!(t.ends_with('…'));
    }

    /// Build a minimal ExtractedFact with the given fact_type string.
    fn fact_with_type(fact_type: &str) -> ExtractedFact {
        ExtractedFact {
            content: "c".to_string(),
            fact_type: fact_type.to_string(),
            subject: None,
            event_date: None,
            event_date_context: None,
            confidence: 0.9,
        }
    }

    /// /154: "experience" maps to FactType::Experience.
    #[test]
    fn to_fact_type_experience() {
        assert_eq!(
            fact_with_type("experience").to_fact_type(),
            crate::storage::FactType::Experience
        );
    }

    /// /154: an unknown fact_type still falls back to Fact (backward-safe).
    #[test]
    fn to_fact_type_unknown_still_falls_back_to_fact() {
        assert_eq!(
            fact_with_type("totally_unknown").to_fact_type(),
            crate::storage::FactType::Fact
        );
    }

    /// /154: the extraction prompt advertises the experience type and its
    /// definition (capital-T "Transferable lesson").
    #[test]
    fn extraction_prompt_contains_experience_type() {
        assert!(EXTRACTION_PROMPT.contains("\"experience\""));
        assert!(EXTRACTION_PROMPT.contains("Transferable lesson"));
    }

    /// /154: FactType::Experience round-trips as the wire value "experience".
    #[test]
    fn fact_type_experience_serde_roundtrip() {
        let json = serde_json::to_string(&crate::storage::FactType::Experience)
            .expect("serialize Experience");
        assert_eq!(json, "\"experience\"");
        let back: crate::storage::FactType =
            serde_json::from_str(&json).expect("deserialize Experience");
        assert_eq!(back, crate::storage::FactType::Experience);
    }

    /// /153 AC-2: no topics ⇒ byte-identical to the inline EXTRACTION_PROMPT.
    #[test]
    fn build_prompt_no_topics_is_byte_identical() {
        let date = "2026-06-11";
        assert_eq!(
            build_extraction_prompt(&cfg(), date),
            EXTRACTION_PROMPT.replace("{conversation_date}", date)
        );
    }

    /// /153 AC-2: an explicitly empty topics list is also byte-identical.
    #[test]
    fn build_prompt_empty_topics_is_byte_identical() {
        let date = "2026-06-11";
        let mut config = cfg();
        config.topics = Some(vec![]);
        assert_eq!(
            build_extraction_prompt(&config, date),
            EXTRACTION_PROMPT.replace("{conversation_date}", date)
        );
    }

    /// /153: operator topics surface their descriptions and wire keys, and
    /// replace the built-in default type list.
    #[test]
    fn build_prompt_with_topics_includes_descriptions() {
        let mut config = cfg();
        config.topics = Some(vec![
            ExtractionTopic {
                name: "risk".to_string(),
                description: "A project risk worth tracking.".to_string(),
                fact_type: Some("risk_item".to_string()),
            },
            ExtractionTopic {
                name: "contact".to_string(),
                description: "A person and how to reach them.".to_string(),
                fact_type: None,
            },
        ]);
        let prompt = build_extraction_prompt(&config, "2026-06-11");
        assert!(prompt.contains("A project risk worth tracking."));
        assert!(prompt.contains("A person and how to reach them."));
        // fact_type key wins when present; name is the fallback wire key.
        assert!(prompt.contains("\"risk_item\""));
        assert!(prompt.contains("\"contact\""));
        // built-in default types are gone when topics are operator-defined.
        assert!(!prompt.contains("Biographical, permanent facts"));
    }

    /// /153: a config carrying topics round-trips through TOML.
    #[test]
    fn config_topics_toml_roundtrip() {
        let mut config = cfg();
        config.topics = Some(vec![ExtractionTopic {
            name: "risk".to_string(),
            description: "desc".to_string(),
            fact_type: Some("risk_item".to_string()),
        }]);
        let toml_str = toml::to_string(&config).expect("serialize config to toml");
        let back: KnowledgeExtractionConfig =
            toml::from_str(&toml_str).expect("deserialize config from toml");
        let topics = back.topics.expect("topics survive roundtrip");
        assert_eq!(topics.len(), 1);
        assert_eq!(topics[0].name, "risk");
        assert_eq!(topics[0].description, "desc");
        assert_eq!(topics[0].fact_type.as_deref(), Some("risk_item"));
    }

    /// /153 AC-6: a config TOML without the topics field deserializes to None.
    #[test]
    fn config_without_topics_field_is_none() {
        let toml_str = r#"
            enabled = true
            model = "gpt-4.1-mini"
            max_transcript_tokens = 20000
            dedup_cosine_threshold = 0.92
            contradiction_check = true
            contradiction_cosine_min = 0.5
            max_facts_per_transcript = 20
        "#;
        let config: KnowledgeExtractionConfig =
            toml::from_str(toml_str).expect("config without topics parses");
        assert!(config.topics.is_none());
    }
}
