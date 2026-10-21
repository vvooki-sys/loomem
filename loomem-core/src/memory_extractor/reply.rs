//! Reject explicitly incomplete completions before admitting extracted facts.

use super::{truncate_reason, ChatHttpReply, ChunkFailure, ExtractedFact, ExtractionResult};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

#[derive(Deserialize)]
struct LlmResponse {
    choices: Vec<LlmChoice>,
}

#[derive(Deserialize)]
struct LlmChoice {
    message: LlmMessage,
    #[serde(default)]
    finish_reason: FinishReason,
}

#[derive(Deserialize)]
struct LlmMessage {
    content: Option<String>,
    #[serde(default)]
    refusal: Option<Value>,
}

/// Missing metadata remains compatible with older OpenAI-compatible providers;
/// explicit null is different: the provider did not report a completed choice.
#[derive(Default)]
enum FinishReason {
    #[default]
    Missing,
    Provided(Value),
}

impl<'de> Deserialize<'de> for FinishReason {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Value::deserialize(deserializer).map(Self::Provided)
    }
}

/// Stable reason reported for a token-limited completion; the split-on-
/// truncation retry keys on it via [`ChunkFailure::is_truncated`].
pub(super) const TRUNCATED_REASON: &str =
    "extraction completion truncated: output token limit reached";

impl ChunkFailure {
    /// `true` when the provider reported `finish_reason: length` for this
    /// chunk (the reply was rejected unparsed, never partially admitted).
    pub fn is_truncated(&self) -> bool {
        self.reason == TRUNCATED_REASON
    }
}

#[derive(Debug, thiserror::Error)]
enum CompletionFailure {
    #[error("{TRUNCATED_REASON}")]
    Truncated,
    #[error("extraction completion refused by provider")]
    Refused,
    #[error("extraction completion did not finish normally")]
    Incomplete,
}

fn validate_completion(choice: &LlmChoice) -> Result<(), CompletionFailure> {
    match &choice.finish_reason {
        FinishReason::Missing => {}
        FinishReason::Provided(Value::String(reason)) if reason == "stop" => {}
        FinishReason::Provided(Value::String(reason)) if reason == "length" => {
            return Err(CompletionFailure::Truncated);
        }
        FinishReason::Provided(Value::String(reason)) if reason == "content_filter" => {
            return Err(CompletionFailure::Refused);
        }
        FinishReason::Provided(_) => return Err(CompletionFailure::Incomplete),
    }
    if choice.message.refusal.is_some() {
        return Err(CompletionFailure::Refused);
    }
    Ok(())
}

/// Parse a 2xx envelope only after the provider's explicit completion metadata
/// allows it. A syntactically valid JSON prefix must not bypass truncation.
pub(super) fn parse_extraction_reply(
    reply: &ChatHttpReply,
    chunk_index: usize,
) -> Result<Vec<ExtractedFact>, ChunkFailure> {
    let failure = |reason| ChunkFailure {
        chunk_index,
        status: Some(reply.status),
        reason,
    };
    let llm_resp: LlmResponse = serde_json::from_str(&reply.body)
        .map_err(|e| failure(truncate_reason(&format!("response parse failed: {e}"))))?;
    let choice = llm_resp
        .choices
        .first()
        .ok_or_else(|| failure("extraction completion has no choice".to_string()))?;
    validate_completion(choice).map_err(|e| failure(e.to_string()))?;
    let content = choice
        .message
        .content
        .as_deref()
        .ok_or_else(|| failure("extraction completion has no content".to_string()))?;
    let json_str = content
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    serde_json::from_str::<ExtractionResult>(json_str)
        .map(|result| result.facts)
        .map_err(|e| failure(truncate_reason(&format!("facts parse failed: {e}"))))
}

#[cfg(test)]
mod tests;
