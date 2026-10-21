//! Source fragments — turn-level slices of a canonical conversation source.
//!
//! Why: a whole session stored as one source chunk never reaches the reader.
//! The local embedder (`local_embeddings.rs`, `multilingual-e5-small`)
//! silently truncates its input at 512 positions — the chunk content as-is
//! (no `query:`/`passage:` prefix is added) plus the tokenizer's two special
//! tokens — so the embedding of a long source only covers its first few
//! hundred tokens, and ranking multipliers then push the source out of the
//! final top-k. This module turns a canonical source into small, quotable
//! fragments — one per transcript turn, long turns split further — that
//! carry the author role and the conversation date, so each fragment can be
//! indexed and quoted on its own.
//!
//! Pure domain code: no I/O, no storage access, no HTTP types. The caller
//! owns persistence (see [`fragment_chunk`]) and indexing. The module never
//! re-sanitises or mutates the source text: `CanonicalSource::text` is the
//! already-sanitised content exactly as persisted, and every fragment's
//! `text` is a byte-identical slice of it.

pub mod config;
mod split;
mod turns;

#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tiktoken_rs::CoreBPE;

use crate::storage::Chunk;
pub use config::{FragmentsConfig, TokenCounterKind, LOCAL_EMBEDDER_WINDOW_TOKENS};

/// The canonical source a fragment set is derived from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CanonicalSource {
    /// Chunk id of the persisted source.
    pub id: String,
    /// Sanitised source content exactly as persisted. Never mutated here.
    pub text: String,
    /// Date the conversation took place (`YYYY-MM-DD`), when known.
    pub conversation_date: Option<String>,
}

/// One fragment of a canonical source. `text == source.text[byte_start..byte_end]`
/// always, and `id == format!("{source_id}:f{index}")`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceFragment {
    pub id: String,
    pub source_id: String,
    /// Position of this fragment within the source's fragment list.
    pub index: u32,
    pub byte_start: usize,
    pub byte_end: usize,
    /// Index of the transcript turn this fragment belongs to.
    pub turn_index: u32,
    /// Normalised author role (`user`, `assistant`, `system`, …) when the
    /// transcript carried markers; `None` for unlabelled text.
    pub role: Option<String>,
    pub conversation_date: Option<String>,
    pub text: String,
}

/// Fragment provenance persisted on a [`Chunk`] (`Chunk::fragment`). A
/// chunk either is a fragment (all of this present) or is not (`None`), so a
/// byte range can never exist without its source id.
///
/// Persisted to RocksDB: new fields must be `#[serde(default)]` (CLAUDE.md
/// hard rule 5).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FragmentMeta {
    /// Chunk id of the canonical source this fragment was cut from.
    pub source_id: String,
    pub byte_start: usize,
    pub byte_end: usize,
    pub turn_index: u32,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub conversation_date: Option<String>,
}

impl From<&SourceFragment> for FragmentMeta {
    fn from(fragment: &SourceFragment) -> Self {
        Self {
            source_id: fragment.source_id.clone(),
            byte_start: fragment.byte_start,
            byte_end: fragment.byte_end,
            turn_index: fragment.turn_index,
            role: fragment.role.clone(),
            conversation_date: fragment.conversation_date.clone(),
        }
    }
}

/// Errors from segmentation. `Invariant` means a bug in this module, not
/// bad input — segmentation never fails on well-formed configuration.
#[derive(Debug, thiserror::Error)]
pub enum FragmentError {
    #[error("fragments.max_fragment_tokens must be positive")]
    InvalidConfig,
    #[error("tokenizer initialization failed: {0}")]
    Tokenizer(String),
    #[error("fragment invariant violated: {0}")]
    Invariant(String),
}

/// Token counting injected into segmentation, so the domain stays
/// independent of the optional `tokenizers` feature and of any one model.
/// A counter that cannot measure a text fails segmentation (`Tokenizer`)
/// rather than guessing — a guess is how hidden truncation gets in.
pub trait TokenCounter {
    fn count(&self, text: &str) -> Result<usize, FragmentError>;
}

impl<T: TokenCounter + ?Sized> TokenCounter for &T {
    fn count(&self, text: &str) -> Result<usize, FragmentError> {
        (**self).count(text)
    }
}

/// Deterministic model-free counter: `ceil(chars / 3)`. Conservative
/// (over-counts) for Latin-script text, where subword tokenisers average
/// three to four chars per token; it under-counts scripts that tokenise
/// near one token per char, so pair it with a budget that leaves slack.
#[derive(Debug, Clone, Copy, Default)]
pub struct ApproxTokenCounter;

impl TokenCounter for ApproxTokenCounter {
    fn count(&self, text: &str) -> Result<usize, FragmentError> {
        Ok(text.chars().count().div_ceil(3))
    }
}

/// `cl100k_base` BPE counter (via `tiktoken-rs`, already a dependency). The
/// encoder is built once per process and shared.
#[derive(Clone, Copy)]
pub struct TiktokenCounter {
    bpe: &'static CoreBPE,
}

impl TiktokenCounter {
    /// Build (or reuse) the process-wide `cl100k_base` encoder.
    pub fn new() -> Result<Self, FragmentError> {
        static BPE: std::sync::OnceLock<CoreBPE> = std::sync::OnceLock::new();
        if let Some(bpe) = BPE.get() {
            return Ok(Self { bpe });
        }
        let built =
            tiktoken_rs::cl100k_base().map_err(|e| FragmentError::Tokenizer(e.to_string()))?;
        Ok(Self {
            bpe: BPE.get_or_init(|| built),
        })
    }
}

impl std::fmt::Debug for TiktokenCounter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TiktokenCounter(cl100k_base)")
    }
}

impl TokenCounter for TiktokenCounter {
    fn count(&self, text: &str) -> Result<usize, FragmentError> {
        Ok(self.bpe.encode_ordinary(text).len())
    }
}

impl TokenCounterKind {
    /// Build the model-free counter this config value names. `Tiktoken` can
    /// fail (encoder construction). `Embedder` is not built here: it wraps
    /// the loaded local embedding model's tokenizer, which only the caller
    /// that owns that model can supply (`local_embeddings::LocalEmbedder::
    /// token_counter`); asking for it without one is an error, never a
    /// fallback to an approximation.
    pub fn counter(self) -> Result<Box<dyn TokenCounter>, FragmentError> {
        Ok(match self {
            Self::Approx => Box::new(ApproxTokenCounter),
            Self::Tiktoken => Box::new(TiktokenCounter::new()?),
            Self::Embedder => {
                return Err(FragmentError::Tokenizer(
                    "embedder tokenizer must be supplied by the caller".to_string(),
                ))
            }
        })
    }
}

/// Segment a canonical source into fragments.
///
/// 1. Turn detection (`turns`): line-prefix markers or JSONL; no markers
///    means one unlabelled turn.
/// 2. Size (`split`): a turn whose `counter.count(text)` exceeds
///    `cfg.max_fragment_tokens` is bisected at paragraph, then sentence,
///    then whitespace boundaries, then on a char boundary.
///
/// Invariants, checked before returning (an `Invariant` error, never a
/// panic): fragments tile `source.text` exactly — sorted, contiguous, no
/// gaps or overlaps, from byte 0 to `text.len()`; each `text` is the exact
/// slice of its range; `index` is the position in the returned vector;
/// ids are unique and equal `"{source_id}:f{index}"`. Budget: every
/// fragment satisfies `counter.count(text)? <= cfg.max_fragment_tokens`,
/// except a fragment that is a single char (it cannot be cut further and
/// is emitted as-is). The function is pure: identical input yields
/// identical output. Empty text yields an empty vector.
pub fn segment_source(
    source: &CanonicalSource,
    cfg: &FragmentsConfig,
    counter: &dyn TokenCounter,
) -> Result<Vec<SourceFragment>, FragmentError> {
    if cfg.max_fragment_tokens == 0 {
        return Err(FragmentError::InvalidConfig);
    }
    if source.text.is_empty() {
        return Ok(Vec::new());
    }
    let mut fragments = Vec::new();
    for (turn_index, turn) in turns::detect_turns(&source.text).iter().enumerate() {
        let turn_index = index_u32(turn_index)?;
        let ranges = split::split_to_budget(
            &source.text,
            turn.start,
            turn.end,
            cfg.max_fragment_tokens,
            counter,
        )?;
        for range in ranges {
            let index = index_u32(fragments.len())?;
            let placement = Placement {
                index,
                turn_index,
                role: turn.role.as_deref(),
            };
            fragments.push(build_fragment(source, range, placement)?);
        }
    }
    verify_invariants(source, &fragments)?;
    Ok(fragments)
}

/// Where a fragment sits: its position and the turn it was cut from.
struct Placement<'a> {
    index: u32,
    turn_index: u32,
    role: Option<&'a str>,
}

fn build_fragment(
    source: &CanonicalSource,
    (byte_start, byte_end): (usize, usize),
    placement: Placement<'_>,
) -> Result<SourceFragment, FragmentError> {
    let text = source
        .text
        .get(byte_start..byte_end)
        .ok_or_else(|| {
            invariant(format!(
                "range {byte_start}..{byte_end} is not on char boundaries"
            ))
        })?
        .to_string();
    Ok(SourceFragment {
        id: fragment_id(&source.id, placement.index),
        source_id: source.id.clone(),
        index: placement.index,
        byte_start,
        byte_end,
        turn_index: placement.turn_index,
        role: placement.role.map(str::to_string),
        conversation_date: source.conversation_date.clone(),
        text,
    })
}

/// Deterministic fragment id: `"{source_id}:f{index}"`.
#[must_use]
pub fn fragment_id(source_id: &str, index: u32) -> String {
    format!("{source_id}:f{index}")
}

fn index_u32(index: usize) -> Result<u32, FragmentError> {
    u32::try_from(index)
        .map_err(|_| FragmentError::Invariant(format!("index {index} does not fit u32")))
}

/// Check the tiling, slice-identity and id invariants documented on
/// [`segment_source`]. Public so callers (and tests) can re-check fragments
/// that crossed a serialization boundary.
pub fn verify_invariants(
    source: &CanonicalSource,
    fragments: &[SourceFragment],
) -> Result<(), FragmentError> {
    let mut cursor = 0usize;
    let mut ids = HashSet::with_capacity(fragments.len());
    for (position, fragment) in fragments.iter().enumerate() {
        verify_fragment(source, fragment, position, cursor)?;
        if !ids.insert(fragment.id.as_str()) {
            return Err(invariant(format!("duplicate fragment id {}", fragment.id)));
        }
        cursor = fragment.byte_end;
    }
    if cursor != source.text.len() {
        return Err(invariant(format!(
            "fragments cover {cursor} of {} bytes",
            source.text.len()
        )));
    }
    Ok(())
}

fn verify_fragment(
    source: &CanonicalSource,
    fragment: &SourceFragment,
    position: usize,
    cursor: usize,
) -> Result<(), FragmentError> {
    let index = index_u32(position)?;
    if fragment.index != index {
        return Err(invariant(format!(
            "fragment at position {position} has index {}",
            fragment.index
        )));
    }
    if fragment.id != fragment_id(&source.id, index) || fragment.source_id != source.id {
        return Err(invariant(format!("fragment {index} has a foreign id")));
    }
    if fragment.byte_start != cursor || fragment.byte_end <= fragment.byte_start {
        return Err(invariant(format!(
            "fragment {index} spans {}..{} after cursor {cursor}",
            fragment.byte_start, fragment.byte_end
        )));
    }
    let slice = source.text.get(fragment.byte_start..fragment.byte_end);
    if slice != Some(fragment.text.as_str()) {
        return Err(invariant(format!(
            "fragment {index} text is not the slice of its range"
        )));
    }
    Ok(())
}

fn invariant(message: String) -> FragmentError {
    FragmentError::Invariant(message)
}

/// Build the chunk for a fragment from a caller-supplied template (stream,
/// timestamps, trust, provenance role, source tag, … are copied as-is; the
/// caller decides them). Sets `id`, `content`, `level = 0`,
/// `source_ids = [source_id]` and `fragment`.
#[must_use]
pub fn fragment_chunk(fragment: &SourceFragment, template: &Chunk) -> Chunk {
    let mut chunk = template.clone();
    chunk.id = fragment.id.clone();
    chunk.content = fragment.text.clone();
    chunk.level = 0;
    chunk.source_ids = Some(vec![fragment.source_id.clone()]);
    chunk.fragment = Some(FragmentMeta::from(fragment));
    chunk
}
