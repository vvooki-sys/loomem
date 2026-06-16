//! Per-channel retrieval signals (cycle/86).
//!
//! Each signal computes a per-candidate raw score; the orchestrator in
//! `super::fusion` ranks candidates per signal and combines them via RRF
//! using the per-type weights produced by `super::query_classifier`.
//!
//! Path A scope (`/86` + `/88`): infrastructure additive only. Signal scoring
//! runs alongside the existing dominant-signal pipeline (`bm25_retrieve` →
//! `vector_retrieve`). Response items remain ordered by the existing
//! pipeline; RRF-fused order + per-signal breakdown surface as a debug
//! field. `/87 per-type eval` measures both rankings side-by-side; the
//! decision to swap the active fusion to RRF is deferred to a follow-up
//! cycle once `/87` validates regression bounds.
//!
//! Four signal kinds per arch §5.1 (excluding `graph_edge_traverse` —
//! deferred to a follow-up cycle; `valid_time_match` removed in /114 Phase 2;
//! `doc_abstract` removed with the file registry in cycle/005):
//! * `Dense` — cosine similarity (raw score from `HybridSearchResult.vector_score`).
//! * `Lexical` — BM25 score (raw score from `HybridSearchResult.bm25_score`).
//! * `EntityMatch` — placeholder in `/86`; full implementation deferred (requires
//!   per-candidate entity-tag fetch from RocksDB).
//! * `Recency` — `exp(-Δt/τ)` decay against `decision_time` (raw score from
//!   `HybridSearchResult.timestamp`).

use serde::Serialize;

pub mod dense;
pub mod entity_match;
pub mod lexical;
pub mod recency;

/// One slot per arch §5.1 channel that participates in /86 fusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalKind {
    Dense,
    Lexical,
    EntityMatch,
    Recency,
}

impl SignalKind {
    /// Iteration order for the fixed slot of `/86` MVP signals.
    pub const ALL: [Self; 4] = [Self::Dense, Self::Lexical, Self::EntityMatch, Self::Recency];
}

/// Per-candidate output of one signal: 1-indexed rank within this signal's
/// ordering and the raw computed score.
///
/// `rank` is `None` if the candidate scored at the floor (e.g. `0.0` for
/// placeholder signals or signals where no temporal/entity match was
/// detected) — RRF treats `None` as "infinity" (zero contribution).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct SignalScore {
    pub rank: Option<u32>,
    pub raw_score: f32,
}

/// Per-result attached breakdown — four signals always present, in the
/// `SignalKind::ALL` order, so `/87` eval can index by slot.
#[derive(Debug, Clone, Serialize)]
pub struct SignalBreakdown {
    pub dense: SignalScore,
    pub lexical: SignalScore,
    pub entity_match: SignalScore,
    pub recency: SignalScore,
}
