//! Layer 1 ambient memory module — `/103a-MVP` foundation.
//!
//! Implements the `POST /v1/ambient` endpoint backbone per `cycles/cycle-103a-
//! layer1-endpoint-brief.md` + §10.7 contract requirements (15) +
//! `/103gate` §8.6 plain-fact constraint + 4 pre-/103a probes empirical
//! findings.
//!
//! Stage 1 of the staged delivery (foundation modules — types, marker, cache).
//! Retrieval + synthesis + handler land in subsequent stages.

pub mod cache;
pub mod marker;
pub mod retrieval;
pub mod synthesis;
pub mod types;

pub use cache::{
    build_key, cache_ttl, count_tokens, truncate_to_budget, AmbientCache, CACHE_DEFAULT_TTL_SECS,
    CACHE_MAX_ENTRIES, TOKEN_BUDGET_HARD_CAP, TOKEN_MARKER_FLOOR, TOKEN_PER_SNIPPET_CAP,
};
pub use marker::{
    build_marker, decide_marker, is_cold_start, MarkerOutcome, SuppressContext,
    COLD_START_GRACE_TURNS,
};
pub use retrieval::{
    apply_layer1_fusion, apply_tombstone_demote, compute_agreement, compute_confidence,
    derive_tier, layer1_fusion_params, layer1_weights, provenance_class_for_level,
    recency_from_breakdown, LAYER1_RECENCY_TAU_DAYS,
};
pub use synthesis::{
    first_sentence, render_conflict_text, render_text, rewrite_first_person,
    synthesize_conflict_snippet, synthesize_snippet, validate_no_metawords,
};
pub use types::{
    AmbientDebug, AmbientLatencyMs, AmbientRequest, AmbientResponse, AmbientSnippet, MarkerIfEmpty,
    NegativeAmbientStatus, NegativeReason, RecentTurn, Tier,
};
