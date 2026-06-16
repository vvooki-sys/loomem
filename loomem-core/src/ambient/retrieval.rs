//! Layer 1 retrieval: §5 RRF reuse with recency-tuned weights.
//!
//! Stage 2 of `cycle/103a-mvp-layer1-endpoint`. Implements:
//!
//! - **AC-7 recency boost** — `τ_recency = 7 days` (vs Layer 2 default 30
//!   days), `recency` weight 0.15 (vs Layer 2 ~0.05).
//! - **AC-8 multi-hop disabled** — `graph_edge` weight forced to 0.0; the
//!   `WeightVector` shape stays identical to §5.1 (7 channels with `graph_edge`
//!   reserved). Per `cycles/CC-PROBES-2026-05-07-pre-103a.md` finding,
//!   `graph_edge_traverse` signal is not actually computed in §5 (deferred per
//!   `signals/mod.rs:15` comment) so structurally the multi-hop is already
//!   off; setting `graph_edge` weight to 0.0 makes the design invariant
//!   explicit and survives if the signal lands later.
//! - **AC-15 type-uniform synthesis** — single `Layer1Weights` constant
//!   applied across all query types, NO `/85 classifier` import or per-type
//!   dispatch. Per `/103gate` close.md §8 #2 empirical justification.
//! - **§10.5.1 confidence formula** — `c = 0.50·rrf_fused + 0.20·provenance
//!   + 0.15·recency + 0.15·agreement`, with tombstone-adjacency demote
//!   capping `c` at 0.55.
//!
//! This module operates over an already-retrieved
//! `&[HybridSearchResult]` slice produced by the handler layer; it does not
//! touch storage, RocksDB, or Tantivy directly. Storage construction stays
//! in `loomem-server` (Stage 3 handler).

use crate::search::fusion::{fuse, FusionParams, FusionResult};
use crate::search::query_taxonomy::WeightVector;
use crate::search::SignalBreakdown;
use crate::HybridSearchResult;

use super::types::Tier;

/// AC-7: 7-day recency half-life for Layer 1 (vs Layer 2 default 30 days).
pub const LAYER1_RECENCY_TAU_DAYS: f64 = 7.0;

/// AC-8: graph signal weight is forced to zero — Layer 1 disables multi-hop.
const LAYER1_GRAPH_EDGE_WEIGHT: f32 = 0.0;

/// AC-7: recency weight value for Layer 1 (vs Layer 2 ~0.05).
const LAYER1_RECENCY_WEIGHT: f32 = 0.15;

/// §10.5.1 weight on RRF-fused score in the confidence formula.
const W_RRF: f32 = 0.50;
/// §10.5.1 weight on provenance class in the confidence formula.
const W_PROVENANCE: f32 = 0.20;
/// §10.5.1 weight on recency signal in the confidence formula.
const W_RECENCY: f32 = 0.15;
/// §10.5.1 weight on agreement signal in the confidence formula.
const W_AGREEMENT: f32 = 0.15;

/// §10.5.1 tombstone-adjacency cap. When the closest supersedes-chain
/// neighbour of the top-1 chunk is a tombstone, demote `c` to at most 0.55
/// (cap at medium tier).
const TOMBSTONE_ADJACENT_CAP: f32 = 0.55;

/// AC-7 + AC-15: single type-uniform weight vector for Layer 1 retrieval.
///
/// Construction: take the `Factual` row as a balanced base (dense + lexical
/// dominant), then force `graph_edge = 0.0` (AC-8) and `recency = 0.15`
/// (AC-7), and renormalize the remaining channels so the row still sums
/// to ≈ 1.0. The exact ratios within `dense / lexical / entity_match` are
/// MVP defaults — final calibration deferred to `/103a-pre` per §10.10 #6.
/// (The former `doc_abstract` share folded into dense/lexical when the file
/// registry was removed in cycle/005.)
#[must_use]
pub fn layer1_weights() -> WeightVector {
    // Sum of non-fixed channels: dense + lexical + entity_match = 0.85
    // After forcing graph=0 + recency=0.15: budget for the three sums cleanly.
    WeightVector {
        dense: 0.345,
        lexical: 0.345,
        entity_match: 0.16,
        graph_edge: LAYER1_GRAPH_EDGE_WEIGHT,
        recency: LAYER1_RECENCY_WEIGHT,
    }
}

/// `FusionParams` for Layer 1 — `recency_tau_days = 7.0`, `now_ts` set by
/// caller's clock, `rrf_k` left at default.
#[must_use]
pub fn layer1_fusion_params(now_ts: i64) -> FusionParams {
    FusionParams {
        now_ts,
        recency_tau_days: LAYER1_RECENCY_TAU_DAYS,
        rrf_k: crate::search::fusion::RRF_K,
    }
}

/// Apply RRF fusion with Layer 1 weights to an already-retrieved candidate
/// list. Wraps `search::fusion::fuse` — the only configuration delta is the
/// weight vector (recency-tuned, graph-zero) and `recency_tau_days = 7.0`.
#[must_use]
pub fn apply_layer1_fusion(candidates: &[HybridSearchResult], now_ts: i64) -> FusionResult {
    fuse(candidates, &layer1_weights(), layer1_fusion_params(now_ts))
}

/// §10.5.1 provenance-class lookup. L0 = direct user statement = 1.0;
/// L1 = consolidated fact = 0.7; L2 = abstract summary = 0.4; anything else
/// (legacy, malformed) defaults to 0.4 to avoid over-confident propagation.
#[must_use]
pub fn provenance_class_for_level(level: i32) -> f32 {
    match level {
        0 => 1.0,
        1 => 0.7,
        2 => 0.4,
        _ => 0.4,
    }
}

/// §10.5.1 agreement signal — MVP heuristic: count how many candidates have
/// `vector_score` within 0.15 of the top-1 candidate. Returns:
/// * `0.0` when only the top-1 candidate (or no corroborators) exists
/// * `0.5` with exactly one corroborator
/// * `1.0` with two or more corroborators
///
/// Real cosine-on-embeddings is a `/103a-pre` calibration item per §10.10
/// (LOW-1 in CC verdict 06d). Vector-score proximity is a structurally
/// available proxy that doesn't require fetching raw embeddings; documented
/// as MVP and replaceable in `/103a-full` without changing the formula.
#[must_use]
pub fn compute_agreement(candidates: &[HybridSearchResult], top1_idx: usize) -> f32 {
    if candidates.is_empty() || top1_idx >= candidates.len() {
        return 0.0;
    }
    let top1_score = candidates[top1_idx].vector_score;
    let corroborators = candidates
        .iter()
        .enumerate()
        .filter(|(i, c)| *i != top1_idx && (c.vector_score - top1_score).abs() < 0.15)
        .count();
    match corroborators {
        0 => 0.0,
        1 => 0.5,
        _ => 1.0,
    }
}

/// §10.5.1 confidence formula. All inputs MUST already be normalized to
/// `[0, 1]` — caller's responsibility (`provenance_class_for_level`,
/// `recency_from_breakdown`, `compute_agreement`).
#[must_use]
pub fn compute_confidence(rrf_fused: f32, provenance: f32, recency: f32, agreement: f32) -> f32 {
    let c = W_RRF * rrf_fused
        + W_PROVENANCE * provenance
        + W_RECENCY * recency
        + W_AGREEMENT * agreement;
    c.clamp(0.0, 1.0)
}

/// Apply §10.5.1 tombstone-adjacency demote: cap `c` at 0.55 (medium tier
/// boundary) when the top-1 chunk's nearest supersedes-chain neighbour is a
/// tombstone. The fact's history is unstable enough that "high" is unsafe.
#[must_use]
pub fn apply_tombstone_demote(c: f32, is_tombstone_adjacent: bool) -> f32 {
    if is_tombstone_adjacent {
        c.min(TOMBSTONE_ADJACENT_CAP)
    } else {
        c
    }
}

/// Pull the recency raw score out of a `SignalBreakdown` and normalize to
/// `[0, 1]`. The §5 recency signal already produces `exp(-Δt/τ)` ∈ [0, 1]
/// directly, so this is a passthrough with a defensive clamp.
#[must_use]
pub fn recency_from_breakdown(breakdown: &SignalBreakdown) -> f32 {
    breakdown.recency.raw_score.clamp(0.0, 1.0)
}

/// Tier derivation. Conflict overrides; otherwise score-based per §10.5.2.
#[must_use]
pub fn derive_tier(c: f32, is_conflict: bool) -> Tier {
    if is_conflict {
        Tier::Conflict
    } else {
        Tier::from_score(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::signals::SignalScore;

    fn breakdown_with_recency(recency_raw: f32) -> SignalBreakdown {
        let zero = SignalScore {
            rank: None,
            raw_score: 0.0,
        };
        SignalBreakdown {
            dense: zero,
            lexical: zero,
            entity_match: zero,
            recency: SignalScore {
                rank: Some(1),
                raw_score: recency_raw,
            },
        }
    }

    fn cand(id: &str, vec_score: f32, level: i32) -> HybridSearchResult {
        HybridSearchResult {
            id: id.to_string(),
            content: "stub".to_string(),
            user_id: "u".to_string(),
            app_id: "a".to_string(),
            level,
            timestamp: 0,
            score: 0.0,
            bm25_score: 0.0,
            vector_score: vec_score,
            time_decay_factor: 1.0,
        }
    }

    #[test]
    fn layer1_weights_force_graph_edge_to_zero() {
        let w = layer1_weights();
        assert_eq!(w.graph_edge, 0.0, "AC-8: multi-hop disabled");
    }

    #[test]
    fn layer1_weights_recency_at_15_percent() {
        let w = layer1_weights();
        assert!(
            (w.recency - 0.15).abs() < 1e-5,
            "AC-7: recency boost = 0.15, got {}",
            w.recency
        );
    }

    #[test]
    fn layer1_weights_sum_to_approximately_one() {
        let w = layer1_weights();
        let sum = w.dense + w.lexical + w.entity_match + w.graph_edge + w.recency;
        assert!(
            (sum - 1.0).abs() < 1e-3,
            "Layer 1 weights must sum ≈1.0, got {sum}"
        );
    }

    #[test]
    fn layer1_fusion_params_use_seven_day_tau() {
        let p = layer1_fusion_params(1_700_000_000);
        assert!(
            (p.recency_tau_days - 7.0).abs() < 1e-9,
            "AC-7: tau=7 days for Layer 1"
        );
        assert_eq!(p.now_ts, 1_700_000_000);
    }

    #[test]
    fn provenance_l0_l1_l2_match_spec() {
        assert_eq!(provenance_class_for_level(0), 1.0);
        assert!((provenance_class_for_level(1) - 0.7).abs() < 1e-6);
        assert!((provenance_class_for_level(2) - 0.4).abs() < 1e-6);
    }

    #[test]
    fn provenance_unknown_level_falls_back_to_l2() {
        // Defensive: legacy/malformed levels demote to L2 baseline.
        assert!((provenance_class_for_level(99) - 0.4).abs() < 1e-6);
        assert!((provenance_class_for_level(-1) - 0.4).abs() < 1e-6);
    }

    #[test]
    fn agreement_zero_when_lone_top1() {
        let cands = vec![cand("a", 0.9, 0)];
        assert_eq!(compute_agreement(&cands, 0), 0.0);
    }

    #[test]
    fn agreement_half_with_one_corroborator() {
        let cands = vec![cand("a", 0.9, 0), cand("b", 0.85, 0)];
        assert!((compute_agreement(&cands, 0) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn agreement_one_with_two_corroborators() {
        let cands = vec![cand("a", 0.9, 0), cand("b", 0.88, 0), cand("c", 0.82, 0)];
        assert!((compute_agreement(&cands, 0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn agreement_skips_distant_candidates() {
        // 0.30 differs from 0.90 by 0.60 — far outside 0.15 proximity window.
        let cands = vec![cand("a", 0.9, 0), cand("b", 0.30, 0)];
        assert_eq!(compute_agreement(&cands, 0), 0.0);
    }

    #[test]
    fn agreement_handles_empty_or_oob_index() {
        let cands: Vec<HybridSearchResult> = vec![];
        assert_eq!(compute_agreement(&cands, 0), 0.0);
        let cands = vec![cand("a", 0.9, 0)];
        assert_eq!(compute_agreement(&cands, 5), 0.0);
    }

    #[test]
    fn confidence_formula_matches_spec_weights() {
        // §10.5.1: c = 0.50·rrf + 0.20·prov + 0.15·rec + 0.15·agree
        // For (1.0, 1.0, 1.0, 1.0) → 1.0 ceiling.
        let c = compute_confidence(1.0, 1.0, 1.0, 1.0);
        assert!((c - 1.0).abs() < 1e-5, "all-ones inputs → c=1.0");

        // For (0, 0, 0, 0) → 0.0.
        assert_eq!(compute_confidence(0.0, 0.0, 0.0, 0.0), 0.0);

        // L0 + recent + corroborated, but mid-RRF: 0.50·0.6 + 0.20·1 + 0.15·0.8 + 0.15·1 = 0.77
        let c = compute_confidence(0.6, 1.0, 0.8, 1.0);
        assert!(
            (c - 0.77).abs() < 1e-5,
            "mixed inputs: expected 0.77, got {c}"
        );
    }

    #[test]
    fn tombstone_demote_caps_at_0_55() {
        assert!((apply_tombstone_demote(0.95, true) - 0.55).abs() < 1e-6);
        assert!((apply_tombstone_demote(0.40, true) - 0.40).abs() < 1e-6);
        assert!((apply_tombstone_demote(0.95, false) - 0.95).abs() < 1e-6);
    }

    #[test]
    fn derive_tier_conflict_overrides_score() {
        assert_eq!(derive_tier(0.99, true), Tier::Conflict);
        assert_eq!(derive_tier(0.0, true), Tier::Conflict);
    }

    #[test]
    fn derive_tier_score_thresholds_match_spec() {
        assert_eq!(derive_tier(0.85, false), Tier::High);
        assert_eq!(derive_tier(0.50, false), Tier::Medium);
        assert_eq!(derive_tier(0.20, false), Tier::Low);
    }

    #[test]
    fn recency_from_breakdown_clamps_into_unit() {
        // §5 recency signal produces exp(-Δt/τ) ∈ [0, 1] but defensively clamp.
        let b = breakdown_with_recency(1.5);
        assert_eq!(recency_from_breakdown(&b), 1.0);
        let b = breakdown_with_recency(-0.2);
        assert_eq!(recency_from_breakdown(&b), 0.0);
        let b = breakdown_with_recency(0.42);
        assert!((recency_from_breakdown(&b) - 0.42).abs() < 1e-6);
    }

    #[test]
    fn apply_layer1_fusion_runs_clean_on_empty_candidates() {
        let result = apply_layer1_fusion(&[], 1_700_000_000);
        assert!(result.fused_order.is_empty());
        assert!(result.breakdowns.is_empty());
        assert!(result.fused_scores.is_empty());
    }
}
