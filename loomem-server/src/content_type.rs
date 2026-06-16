//! Server-side LLM classifier for content-type (ADR-017, cycle/142).
//!
//! The domain (`loomem_core::content_type`) is HTTP-agnostic: it talks to the
//! LLM through the [`ContentTypeClassifier`] trait. This module supplies the
//! production implementation ([`HttpContentTypeClassifier`], wrapping `reqwest`
//! and the OpenAI chat endpoint), so the domain stays free of `reqwest` (the
//! trait is the seam — CLAUDE.md §4, ADR-014). Mirrors `HttpManifestCompleter`.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use loomem_core::config::LlmConfig;
use loomem_core::content_type::{ContentType, ContentTypeClassifier};

/// Max content bytes sent to the classifier. The form of a document is evident
/// from its head; capping keeps token cost bounded.
const MAX_INPUT_BYTES: usize = 4000;

/// Frozen v1 prompt. The model must return exactly one snake_case label.
const SYSTEM_PROMPT: &str = "You classify the FORM of a document (not its topic) into EXACTLY ONE content type. \
Reply with ONLY the snake_case label, nothing else. Valid labels: \
operational_instruction, policy, changelog, case_study, article, person_profile, index, org_fact, technical_project, other. \
Definitions: operational_instruction=how-to/steps; policy=normative rule/MUST/ban; changelog=what changed/versions; \
case_study=client project write-up; article=opinion/thought-leadership; person_profile=who someone is; \
index=list of pointers; org_fact=organizational fact (client, award, structure); technical_project=architecture/code/cycle; \
other=none of the above. If unsure, reply other.";

/// Production [`ContentTypeClassifier`] — one OpenAI chat completion.
pub(crate) struct HttpContentTypeClassifier {
    client: reqwest::Client,
    api_key: Option<String>,
    model: String,
    timeout_secs: u64,
}

impl HttpContentTypeClassifier {
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

fn truncate_to_char_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

impl ContentTypeClassifier for HttpContentTypeClassifier {
    fn classify(
        &self,
        content: &str,
    ) -> impl std::future::Future<Output = Result<ContentType>> + Send {
        // Clone captures so the future owns them (Send, no borrow across await).
        let client = self.client.clone();
        let api_key = self.api_key.clone();
        let model = self.model.clone();
        let timeout = self.timeout_secs;
        let input = truncate_to_char_boundary(content, MAX_INPUT_BYTES).to_string();

        async move {
            let api_key = api_key.context("OpenAI API key not configured for content-type")?;
            let request_body = serde_json::json!({
                "model": model,
                "messages": [
                    {"role": "system", "content": SYSTEM_PROMPT},
                    {"role": "user", "content": input}
                ],
                "max_tokens": 10,
                "temperature": 0.0
            });

            let response = client
                .post("https://api.openai.com/v1/chat/completions")
                .header("Authorization", format!("Bearer {api_key}"))
                .header("Content-Type", "application/json")
                .json(&request_body)
                .timeout(Duration::from_secs(timeout))
                .send()
                .await
                .context("Failed to send content-type classification request")?;

            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                anyhow::bail!("content-type LLM call failed ({status}): {body}");
            }

            let parsed: ChatResponse = response
                .json()
                .await
                .context("Failed to parse content-type LLM response")?;
            let label = parsed
                .choices
                .into_iter()
                .next()
                .map(|c| c.message.content)
                .unwrap_or_default();
            // Unrecognized label degrades to `other` (the band stays low either
            // way; do not fail the classification on a chatty model).
            Ok(ContentType::parse(&label).unwrap_or(ContentType::Other))
        }
    }
}
