//! Configuration for source fragmentation.
//!
//! `FragmentsConfig` is a sub-config owned by the `fragments` module
//! (CLAUDE.md §Rust rules: one `*Config` per module). Composed into the root
//! `Config` with `#[serde(default)]`, so existing `config.toml` files without
//! a `[fragments]` section load and degrade to **off** — byte-identical
//! behavior for existing deployments and clients.

use serde::{Deserialize, Serialize};

/// Default budget (in tokens) for a fragment's text. Chosen so that the text
/// plus the embedding prefix stays well inside the 512-position window of
/// the local embedder (`multilingual-e5-small`, see `local_embeddings.rs`),
/// which silently truncates longer inputs.
pub const DEFAULT_MAX_FRAGMENT_TOKENS: usize = 400;

/// Default number of tokens kept free for the embedding-side prefix
/// (`"passage: "` plus special tokens) on top of the text budget.
pub const DEFAULT_PREFIX_RESERVE_TOKENS: usize = 16;

/// Source fragmentation settings. Default off.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FragmentsConfig {
    /// When `true`, canonical sources may be segmented into fragments. When
    /// `false` (default) nothing is fragmented and the pipeline behaves
    /// exactly as before this module existed.
    #[serde(default)]
    pub enabled: bool,
    /// Budget a fragment's TEXT may occupy, as measured by the injected
    /// `TokenCounter`. Every fragment satisfies `count(text) <= max_fragment_tokens`
    /// except the unsplittable single-character case (see `segment_source`).
    #[serde(default = "default_max_fragment_tokens")]
    pub max_fragment_tokens: usize,
    /// Tokens reserved for the embedding prefix. Not consumed by segmentation
    /// itself; `window_tokens()` exposes `max_fragment_tokens + prefix_reserve_tokens`
    /// so the caller can check it against the embedder's window.
    #[serde(default = "default_prefix_reserve_tokens")]
    pub prefix_reserve_tokens: usize,
}

fn default_max_fragment_tokens() -> usize {
    DEFAULT_MAX_FRAGMENT_TOKENS
}

fn default_prefix_reserve_tokens() -> usize {
    DEFAULT_PREFIX_RESERVE_TOKENS
}

impl Default for FragmentsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_fragment_tokens: DEFAULT_MAX_FRAGMENT_TOKENS,
            prefix_reserve_tokens: DEFAULT_PREFIX_RESERVE_TOKENS,
        }
    }
}

impl FragmentsConfig {
    /// Total tokens an embedded fragment may occupy: text budget plus the
    /// prefix reserve. Must not exceed the embedding model's input window
    /// (512 for `multilingual-e5-small`).
    #[must_use]
    pub fn window_tokens(&self) -> usize {
        self.max_fragment_tokens
            .saturating_add(self.prefix_reserve_tokens)
    }
}
