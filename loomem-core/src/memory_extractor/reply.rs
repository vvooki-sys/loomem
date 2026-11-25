//! Reject explicitly incomplete completions before admitting extracted facts.

use super::{truncate_reason, ChatHttpReply, ChunkFailure, ExtractedFact, ExtractionResult};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use tracing::warn;

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

/// Longest tail (whitespace trimmed) that `tolerate_trailing_json` discards
/// after the first complete facts object. Sized for stray closers the model
/// appends after a finished object (the observed case was 2 characters, `]}`);
/// anything longer, or any character outside `}` / `]` / whitespace, keeps
/// the strict parse error so real syntax damage is never masked.
pub(super) const MAX_TOLERATED_TAIL_CHARS: usize = 8;

/// `true` when `tail` is only stray closers the salvage path may drop.
fn is_tolerable_tail(tail: &str) -> bool {
    let trimmed = tail.trim();
    !trimmed.is_empty()
        && trimmed.chars().count() <= MAX_TOLERATED_TAIL_CHARS
        && trimmed
            .chars()
            .all(|c| c == '}' || c == ']' || c.is_whitespace())
}

/// Salvage path for a reply the strict parse rejected: take the first
/// complete JSON value from the start of `json_str` and accept it only when
/// what follows is a tolerable tail. `None` for every other failure, so the
/// caller reports the original strict error unchanged.
fn salvage_leading_object(json_str: &str, chunk_index: usize) -> Option<ExtractionResult> {
    let mut stream = serde_json::Deserializer::from_str(json_str).into_iter::<ExtractionResult>();
    let result = stream.next()?.ok()?;
    let tail = json_str.get(stream.byte_offset()..)?;
    if !is_tolerable_tail(tail) {
        return None;
    }
    // Length only: the tail is model output, never logged verbatim (rule 1).
    warn!(
        "Extraction chunk {chunk_index}: discarded {} trailing character(s) after the facts object (tolerate_trailing_json)",
        tail.trim().chars().count()
    );
    Some(result)
}

/// Strict parse first (byte-identical to the pre-flag path); with
/// `tolerate_trailing_json` a rejected reply gets one salvage attempt that
/// only drops a short tail of stray closers.
fn parse_facts(
    json_str: &str,
    chunk_index: usize,
    tolerate_trailing_json: bool,
) -> Result<ExtractionResult, serde_json::Error> {
    match serde_json::from_str::<ExtractionResult>(json_str) {
        Ok(result) => Ok(result),
        Err(e) if tolerate_trailing_json => salvage_leading_object(json_str, chunk_index).ok_or(e),
        Err(e) => Err(e),
    }
}

/// Parse a 2xx envelope only after the provider's explicit completion metadata
/// allows it. A syntactically valid JSON prefix must not bypass truncation.
pub(super) fn parse_extraction_reply(
    reply: &ChatHttpReply,
    chunk_index: usize,
    tolerate_trailing_json: bool,
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
    parse_facts(json_str, chunk_index, tolerate_trailing_json)
        .map(|result| result.facts)
        .map_err(|e| failure(truncate_reason(&format!("facts parse failed: {e}"))))
}

#[cfg(test)]
mod tests;
