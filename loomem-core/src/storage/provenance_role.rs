use serde::{Deserialize, Serialize};

/// Cycle /001 (MemIR): provenance role of a chunk — how authoritative its
/// content is as a retrieval signal. Combined with `trust_level` into a
/// multiplier applied to the fused score in `hybrid_search`, so equally
/// relevant chunks are ordered by authority rather than tied.
///
/// `Claim` remains the backward-compatible default. Validated ingestion
/// sources use `Evidence`; extracted assertions use `Claim`. `Cue` is
/// reserved for weak associative signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ProvenanceRole {
    /// A first-class asserted fact/memory. Highest retrieval weight (1.00).
    #[default]
    Claim,
    /// Supporting evidence derived from other content. Lowest retrieval weight (0.50).
    Evidence,
    /// A weak associative cue (e.g. graph/entity link). Reduced retrieval weight (0.80).
    Cue,
}
