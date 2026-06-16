//! Configuration for content-type classification (ADR-017, cycle/142 + /143).
//!
//! `ContentTypeConfig` is a sub-config owned by the `content_type` module
//! (CLAUDE.md §5). Composed into the root `Config` with `#[serde(default)]`, so
//! existing `config.toml` files without a `[content_type]` section load and
//! degrade to typing-off (no LLM HTTP, no sidecar entries).

use serde::{Deserialize, Serialize};

/// Per-instance content-type classification config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentTypeConfig {
    /// Typing on/off (since /143 the LLM is the only classifier). When false,
    /// `classify_content` returns `None`: writes create no sidecar entry and
    /// reads show no tag. Default false (no HTTP, no cost).
    pub enabled: bool,
    /// Model used for the LLM classification completion. Part of the cache key,
    /// so a model change invalidates previously cached classifications
    /// (wzorzec /90).
    pub model: String,
}

impl Default for ContentTypeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: "gpt-4.1-mini".to_string(),
        }
    }
}

impl ContentTypeConfig {
    /// Apply env var overrides (Railway per-instance enable). Env takes
    /// precedence over `config.toml`. The baked image carries no
    /// `[content_type]` section (default `enabled=false`), so
    /// `LOOMEM_CONTENT_TYPE_ENABLED=true` is how an operator turns typing on for
    /// a single instance without a global config edit.
    pub fn apply_env_overrides(&mut self) {
        if let Ok(v) = std::env::var("LOOMEM_CONTENT_TYPE_ENABLED") {
            match v.as_str() {
                "true" | "1" => self.enabled = true,
                "false" | "0" => self.enabled = false,
                // Empty / unknown → keep current value + WARN (avoid silent
                // regression on a typo like "True").
                other => tracing::warn!(
                    "LOOMEM_CONTENT_TYPE_ENABLED={:?} not recognized (expected true/false/1/0), keeping current value {}",
                    other,
                    self.enabled
                ),
            }
        }
    }
}
