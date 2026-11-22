//! Per-provider chat request options (`[llm.providers.<name>]`).
//!
//! Every chat call site builds the same OpenAI-style body (`model`,
//! `messages`, `max_tokens`, usually `temperature`). A provider entry can
//! declare how that body must differ for its models: drop `temperature`,
//! send the output cap as `max_completion_tokens`, raise small caps to a
//! floor, add `reasoning_effort`. All options default to off, and
//! [`ChatCapabilities::apply`] returns a body untouched when nothing is set —
//! in particular for the built-in `openai` provider without a table entry —
//! so existing requests stay byte-identical.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// `reasoning_effort` values the Chat Completions API documents
/// (developers.openai.com/api/reference, "Create chat completion",
/// accessed 2026-11-22). The list is the union over the API, not what any
/// one model accepts: a model may reject part of it with a 400 — gpt-6-luna
/// takes `none`, `low`, `medium`, `high`, `xhigh` and rejects `minimal`
/// (API response, 2026-11-22). Other OpenAI-compatible providers may accept
/// other subsets, so the list is deliberately not narrowed to one model; the
/// config check only rejects values outside it.
pub const REASONING_EFFORTS: &[&str] =
    &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Request options of one provider entry. Flattened into
/// [`super::LlmProviderConfig`], so the keys sit directly under
/// `[llm.providers.<name>]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatCapabilities {
    /// Drop `temperature` from every chat request (models with a fixed
    /// temperature, or reasoning models that reject a non-default value).
    #[serde(default)]
    pub omit_temperature: bool,
    /// Send the output cap as `max_completion_tokens` instead of the
    /// deprecated `max_tokens`.
    #[serde(default)]
    pub max_completion_tokens: bool,
    /// Raise any per-call output cap below this value to it. `0` leaves the
    /// caps unchanged.
    #[serde(default)]
    pub completion_tokens_floor: u32,
    /// Added as `reasoning_effort` to every chat request when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// The output cap of one request after [`ChatCapabilities`] were applied.
/// Exactly one of the two fields is `Some`; the typed request structs
/// serialize whichever is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompletionLimit {
    pub max_tokens: Option<u32>,
    pub max_completion_tokens: Option<u32>,
}

impl CompletionLimit {
    /// The cap that was sent, for error messages.
    pub fn value(self) -> u32 {
        self.max_tokens.or(self.max_completion_tokens).unwrap_or(0)
    }
}

impl ChatCapabilities {
    /// `true` when no option is set: [`Self::apply`] is the identity.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Static config check: `reasoning_effort`, when set, is one of
    /// [`REASONING_EFFORTS`].
    pub fn validate(&self, provider: &str) -> anyhow::Result<()> {
        if let Some(effort) = &self.reasoning_effort {
            anyhow::ensure!(
                REASONING_EFFORTS.contains(&effort.as_str()),
                "llm.providers.{provider}.reasoning_effort {effort:?} must be one of {}",
                REASONING_EFFORTS.join(", ")
            );
        }
        Ok(())
    }

    /// The cap to send for a call site that asks for `requested` output
    /// tokens: raised to `completion_tokens_floor`, under the configured key.
    pub fn completion_limit(&self, requested: u32) -> CompletionLimit {
        let cap = requested.max(self.completion_tokens_floor);
        if self.max_completion_tokens {
            CompletionLimit {
                max_tokens: None,
                max_completion_tokens: Some(cap),
            }
        } else {
            CompletionLimit {
                max_tokens: Some(cap),
                max_completion_tokens: None,
            }
        }
    }

    /// The `temperature` to send for a call site that asks for `requested`:
    /// `None` when the provider omits it.
    pub fn temperature(&self, requested: f32) -> Option<f32> {
        (!self.omit_temperature).then_some(requested)
    }

    /// Rewrite a `serde_json` chat body. With nothing set the body comes back
    /// untouched (same `Value`, same bytes). Otherwise: `temperature` is
    /// removed when `omit_temperature`; a `max_tokens` (or an existing
    /// `max_completion_tokens`) below `completion_tokens_floor` is raised;
    /// `max_tokens` moves to `max_completion_tokens` when that flag is set;
    /// `reasoning_effort` is inserted when set.
    pub fn apply(&self, mut body: Value) -> Value {
        if self.is_default() {
            return body;
        }
        let Some(obj) = body.as_object_mut() else {
            return body;
        };
        if self.omit_temperature {
            obj.remove("temperature");
        }
        let floor = u64::from(self.completion_tokens_floor);
        if let Some(cap) = obj.get("max_tokens").and_then(Value::as_u64) {
            let cap = cap.max(floor);
            if self.max_completion_tokens {
                obj.remove("max_tokens");
                obj.insert("max_completion_tokens".to_string(), json!(cap));
            } else {
                obj.insert("max_tokens".to_string(), json!(cap));
            }
        } else if let Some(cap) = obj.get("max_completion_tokens").and_then(Value::as_u64) {
            obj.insert("max_completion_tokens".to_string(), json!(cap.max(floor)));
        }
        if let Some(effort) = &self.reasoning_effort {
            obj.insert("reasoning_effort".to_string(), json!(effort));
        }
        body
    }
}

/// The output cap a `serde_json` chat body carries (`max_completion_tokens`
/// or `max_tokens`), for error messages. `0` when neither is present.
pub fn completion_limit_of(body: &Value) -> u32 {
    ["max_completion_tokens", "max_tokens"]
        .iter()
        .find_map(|key| body.get(key).and_then(Value::as_u64))
        .and_then(|cap| u32::try_from(cap).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> Value {
        json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 100,
            "temperature": 0.0
        })
    }

    #[test]
    fn default_capabilities_leave_the_body_untouched() {
        let caps = ChatCapabilities::default();
        assert!(caps.is_default());
        let before = body();
        let after = caps.apply(before.clone());
        assert_eq!(after, before);
        assert_eq!(
            serde_json::to_string(&after).unwrap(),
            serde_json::to_string(&before).unwrap()
        );
        // A non-object body is returned as is too.
        assert_eq!(caps.apply(json!("x")), json!("x"));
        assert_eq!(caps.completion_limit(100).max_tokens, Some(100));
        assert_eq!(caps.completion_limit(100).max_completion_tokens, None);
        assert_eq!(caps.temperature(0.0), Some(0.0));
        assert_eq!(caps.reasoning_effort, None);
    }

    #[test]
    fn omit_temperature_removes_only_temperature() {
        let caps = ChatCapabilities {
            omit_temperature: true,
            ..Default::default()
        };
        let after = caps.apply(body());
        assert!(after.get("temperature").is_none());
        assert_eq!(after["max_tokens"], 100);
        assert_eq!(after["model"], "m");
        assert_eq!(caps.temperature(0.3), None);
        // A body without temperature is unchanged.
        let mut without = body();
        without.as_object_mut().unwrap().remove("temperature");
        assert_eq!(caps.apply(without.clone()), without);
    }

    #[test]
    fn max_completion_tokens_moves_the_cap() {
        let caps = ChatCapabilities {
            max_completion_tokens: true,
            ..Default::default()
        };
        let after = caps.apply(body());
        assert!(after.get("max_tokens").is_none());
        assert_eq!(after["max_completion_tokens"], 100);
        assert_eq!(after["temperature"], 0.0);
        let limit = caps.completion_limit(100);
        assert_eq!(limit.max_tokens, None);
        assert_eq!(limit.max_completion_tokens, Some(100));
        assert_eq!(limit.value(), 100);
    }

    #[test]
    fn floor_raises_small_caps_and_keeps_large_ones() {
        let caps = ChatCapabilities {
            completion_tokens_floor: 500,
            ..Default::default()
        };
        assert_eq!(caps.apply(body())["max_tokens"], 500);
        let mut large = body();
        large["max_tokens"] = json!(2000);
        assert_eq!(caps.apply(large)["max_tokens"], 2000);
        let mut already_new_key = body();
        already_new_key
            .as_object_mut()
            .unwrap()
            .remove("max_tokens");
        already_new_key["max_completion_tokens"] = json!(10);
        let after = caps.apply(already_new_key);
        assert_eq!(after["max_completion_tokens"], 500);
        assert!(after.get("max_tokens").is_none());
        assert_eq!(caps.completion_limit(10).max_tokens, Some(500));
        assert_eq!(caps.completion_limit(2000).max_tokens, Some(2000));
    }

    #[test]
    fn reasoning_effort_is_inserted_only_when_set() {
        let caps = ChatCapabilities {
            reasoning_effort: Some("low".into()),
            ..Default::default()
        };
        assert_eq!(caps.apply(body())["reasoning_effort"], "low");
        assert!(ChatCapabilities::default()
            .apply(body())
            .get("reasoning_effort")
            .is_none());
    }

    #[test]
    fn reasoning_provider_combination() {
        let caps = ChatCapabilities {
            omit_temperature: true,
            max_completion_tokens: true,
            completion_tokens_floor: 2000,
            reasoning_effort: Some("low".into()),
        };
        let after = caps.apply(body());
        assert_eq!(
            after,
            json!({
                "model": "m",
                "messages": [{"role": "user", "content": "hi"}],
                "max_completion_tokens": 2000,
                "reasoning_effort": "low"
            })
        );
        assert_eq!(completion_limit_of(&after), 2000);
        assert_eq!(completion_limit_of(&body()), 100);
        assert_eq!(completion_limit_of(&json!({})), 0);
    }

    #[test]
    fn validate_accepts_documented_efforts_only() {
        for effort in REASONING_EFFORTS {
            let caps = ChatCapabilities {
                reasoning_effort: Some((*effort).to_string()),
                ..Default::default()
            };
            caps.validate("p").expect(effort);
        }
        assert!(ChatCapabilities::default().validate("p").is_ok());
        for bad in ["", "LOW", "extreme"] {
            let caps = ChatCapabilities {
                reasoning_effort: Some(bad.to_string()),
                ..Default::default()
            };
            let err = caps.validate("p").expect_err(bad).to_string();
            assert!(err.starts_with("llm.providers.p.reasoning_effort"), "{err}");
        }
    }

    #[test]
    fn parses_from_toml_with_defaults() {
        let parsed: ChatCapabilities = toml::from_str("").unwrap();
        assert!(parsed.is_default());
        let parsed: ChatCapabilities = toml::from_str(
            "omit_temperature = true\nmax_completion_tokens = true\ncompletion_tokens_floor = 2000\nreasoning_effort = \"low\"\n",
        )
        .unwrap();
        assert_eq!(parsed.completion_tokens_floor, 2000);
        assert_eq!(parsed.reasoning_effort.as_deref(), Some("low"));
    }
}
