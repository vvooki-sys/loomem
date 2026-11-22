//! Reject a truncated chat completion before its content is used.
//!
//! A reply with `finish_reason: "length"` hit the output cap: the content is
//! cut mid-sentence or mid-JSON and was never a valid result, so every chat
//! call site treats it as an error instead of parsing what arrived. Knowledge
//! extraction (`memory_extractor::reply`) and the evidence selector keep
//! their own, stricter checks.

use serde_json::Value;

/// The reply ended because the model hit the output cap.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "chat completion truncated: finish_reason={finish_reason}, model={model}, completion token limit={limit}"
)]
pub struct TruncatedCompletion {
    pub finish_reason: String,
    pub model: String,
    /// The cap the request carried (`max_tokens` or `max_completion_tokens`).
    pub limit: u32,
}

/// `Err` when `finish_reason` is `"length"`. A missing reason (older
/// OpenAI-compatible servers) and every other value pass.
pub fn ensure_not_truncated(
    finish_reason: Option<&str>,
    model: &str,
    limit: u32,
) -> Result<(), TruncatedCompletion> {
    match finish_reason {
        Some("length") => Err(TruncatedCompletion {
            finish_reason: "length".to_string(),
            model: model.to_string(),
            limit,
        }),
        _ => Ok(()),
    }
}

/// [`ensure_not_truncated`] for a response parsed as a `serde_json` value:
/// reads `choices[0].finish_reason`.
pub fn ensure_choice_not_truncated(
    response: &Value,
    model: &str,
    limit: u32,
) -> Result<(), TruncatedCompletion> {
    let reason = response
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str);
    ensure_not_truncated(reason, model, limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn length_is_an_error_naming_reason_model_and_limit() {
        let err = ensure_not_truncated(Some("length"), "gpt-6-luna", 500).unwrap_err();
        assert_eq!(
            err.to_string(),
            "chat completion truncated: finish_reason=length, model=gpt-6-luna, completion token limit=500"
        );
    }

    #[test]
    fn stop_missing_and_other_reasons_pass() {
        for reason in [
            Some("stop"),
            None,
            Some("tool_calls"),
            Some("content_filter"),
        ] {
            ensure_not_truncated(reason, "m", 1).expect("not truncated");
        }
    }

    #[test]
    fn value_form_reads_the_first_choice() {
        let truncated =
            json!({"choices": [{"finish_reason": "length", "message": {"content": "{"}}]});
        assert!(ensure_choice_not_truncated(&truncated, "m", 10).is_err());
        let stopped = json!({"choices": [{"finish_reason": "stop", "message": {"content": "{}"}}]});
        ensure_choice_not_truncated(&stopped, "m", 10).unwrap();
        let legacy = json!({"choices": [{"message": {"content": "{}"}}]});
        ensure_choice_not_truncated(&legacy, "m", 10).unwrap();
        ensure_choice_not_truncated(&json!({}), "m", 10).unwrap();
    }
}
