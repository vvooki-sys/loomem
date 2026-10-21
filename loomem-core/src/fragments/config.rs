//! Configuration for source fragmentation.
//!
//! `FragmentsConfig` is a sub-config owned by the `fragments` module
//! (CLAUDE.md §Rust rules: one `*Config` per module). Composed into the root
//! `Config` with `#[serde(default)]`, so existing `config.toml` files without
//! a `[fragments]` section load and degrade to **off** — byte-identical
//! behavior for existing deployments and clients.

use crate::storage::ProvenanceRole;
use serde::{Deserialize, Serialize};

/// Default budget (in tokens) for a fragment's text. Chosen so that the text
/// plus the reserve stays well inside the 512-position window of the local
/// embedder (`multilingual-e5-small`, see `local_embeddings.rs`), which
/// silently truncates longer inputs.
pub const DEFAULT_MAX_FRAGMENT_TOKENS: usize = 400;

/// Default number of tokens kept free on top of the text budget. The
/// embedder adds exactly two special tokens (`<s>`, `</s>`) to the content —
/// no `"passage: "` prefix is prepended anywhere in Loomem — so the rest of
/// the reserve is headroom for the gap between the configured counter and
/// the embedder's own tokenizer. (The key keeps its historical name
/// `prefix_reserve_tokens` for config compatibility.)
pub const DEFAULT_PREFIX_RESERVE_TOKENS: usize = 16;

/// Input window of the local embedder (`multilingual-e5-small`): 512
/// positions including the two special tokens. Mirrors
/// `DEFAULT_MAX_SEQUENCE_TOKENS` in `local_embeddings.rs`.
pub const LOCAL_EMBEDDER_WINDOW_TOKENS: usize = 512;

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
    /// Tokens kept free above the text budget: the embedder's two special
    /// tokens plus headroom (there is no embedding prefix; the name is kept
    /// for compatibility). Not consumed by segmentation itself;
    /// `window_tokens()` exposes `max_fragment_tokens + prefix_reserve_tokens`
    /// so `Config::validate` can check it against the embedder's window.
    #[serde(default = "default_prefix_reserve_tokens")]
    pub prefix_reserve_tokens: usize,
    /// Provenance role stamped on every persisted fragment chunk — the
    /// ranking knob for the fragments experiment (`hybrid_search` applies a
    /// per-role multiplier). Written in `config.toml` as the lowercase name:
    /// `"claim"` (default), `"evidence"` or `"cue"`; any other value is a
    /// config load error.
    #[serde(default, with = "role_name")]
    pub provenance_role: ProvenanceRole,
    /// Token counter segmentation measures fragments with: `"approx"`
    /// (default, model-free `ceil(chars / 3)`), `"tiktoken"` (`cl100k_base`
    /// BPE) or `"embedder"` (the loaded local embedding model's own
    /// tokenizer — the only counter whose budget is the embedder's; requires
    /// `embedding_provider = "local"` with the model loaded, otherwise
    /// segmentation fails and the tool text says so). Any other value is a
    /// config load error.
    #[serde(default)]
    pub token_counter: TokenCounterKind,
}

/// Which [`super::TokenCounter`] segmentation uses. Lowercase in config.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum TokenCounterKind {
    /// [`super::ApproxTokenCounter`]: deterministic, no model.
    #[default]
    Approx,
    /// [`super::TiktokenCounter`]: `cl100k_base` BPE.
    Tiktoken,
    /// The local embedder's tokenizer (`local_embeddings::LocalEmbedder::
    /// token_counter`), counting special tokens like the embedder does.
    Embedder,
}

/// Lowercase config spelling of [`ProvenanceRole`] (`claim` / `evidence` /
/// `cue`); the enum's own serde form is the capitalised variant name used in
/// stored chunks, which is not what a config file should have to spell.
mod role_name {
    use super::ProvenanceRole;
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(role: &ProvenanceRole, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(match role {
            ProvenanceRole::Claim => "claim",
            ProvenanceRole::Evidence => "evidence",
            ProvenanceRole::Cue => "cue",
        })
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<ProvenanceRole, D::Error> {
        match String::deserialize(d)?.as_str() {
            "claim" => Ok(ProvenanceRole::Claim),
            "evidence" => Ok(ProvenanceRole::Evidence),
            "cue" => Ok(ProvenanceRole::Cue),
            other => Err(D::Error::custom(format!(
                "fragments.provenance_role must be one of claim, evidence, cue (got {other:?})"
            ))),
        }
    }
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
            provenance_role: ProvenanceRole::Claim,
            token_counter: TokenCounterKind::Approx,
        }
    }
}

impl FragmentsConfig {
    /// Total tokens an embedded fragment may occupy: text budget plus the
    /// reserve. Must not exceed the embedding model's input window
    /// ([`LOCAL_EMBEDDER_WINDOW_TOKENS`] for `multilingual-e5-small`).
    #[must_use]
    pub fn window_tokens(&self) -> usize {
        self.max_fragment_tokens
            .saturating_add(self.prefix_reserve_tokens)
    }

    /// Startup checks (called from `Config::validate`). `local_provider` is
    /// whether `llm.embedding_provider` is `"local"`, the only provider whose
    /// window Loomem knows.
    pub fn validate(&self, local_provider: bool) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.max_fragment_tokens > 0,
            "fragments.max_fragment_tokens must be positive"
        );
        if local_provider {
            anyhow::ensure!(
                self.window_tokens() <= LOCAL_EMBEDDER_WINDOW_TOKENS,
                "fragments.max_fragment_tokens + fragments.prefix_reserve_tokens ({}) must not exceed \
                 the local embedder's {LOCAL_EMBEDDER_WINDOW_TOKENS}-token window",
                self.window_tokens()
            );
        }
        Ok(())
    }
}
