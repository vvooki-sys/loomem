//! Query taxonomy + classification for retrieval routing (cycle/85).
//!
//! Architecture: deterministic regex parser emits `(QueryType, WeightVector,
//! ParsedFeatures)` for each query. Wagi są placeholder tier values na MVP;
//! finalne decimals z `/87 per-type eval`. Hard rule: zero LLM call w hot path.
//!
//! See: `docs/architecture/memory-routing.md` §3 + §4 + §5.

pub mod fusion;
pub mod query_classifier;
pub mod query_taxonomy;
pub mod rare_term;
pub mod signals;
pub mod tier1;

pub use fusion::{fuse, FusionParams, FusionResult};
pub use query_classifier::classify;
pub use query_taxonomy::{ClassifiedQuery, ParsedFeatures, QueryType, WeightVector};
pub use rare_term::{rare_df_threshold, select_rare_tokens, RareTermLaneConfig, RareToken};
pub use signals::{SignalBreakdown, SignalKind, SignalScore};
pub use tier1::Tier1Config;
