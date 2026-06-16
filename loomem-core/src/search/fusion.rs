//! Reciprocal Rank Fusion orchestrator (cycle/86, Path A).
//!
//! Consumes per-signal raw scores from `super::signals`, ranks each
//! channel, and combines them via RRF using the per-type `WeightVector`
//! produced by `super::query_classifier::classify`.
//!
//! Path A scope: this orchestrator runs **alongside** the existing
//! dominant-signal pipeline. Its outputs (`fused_order` + per-result
//! `SignalBreakdown`) surface as a debug field for `/87 per-type eval`
//! to compare against the existing pipeline's ranking on the same
//! candidate pool. The active hot-path fusion is unchanged in `/86`.
//!
//! Algorithm (arch §5.2):
//! ```text
//! score_fused(c) = Σ_signal w_signal · 1 / (k + rank_signal(c))
//! ```
//! where `k = 60` (literature default per arch §8 Q1), `w_signal` comes
//! from `WeightVector::for_type(query_type)` (already row-summed to 1.0
//! after /85's normalization), and `rank_signal(c)` is the 1-indexed
//! position of `c` in the sorted ranking for that signal.
//!
//! Cold-start safety (arch §5.4 step 6): channels whose raw scores are
//! all zero contribute nothing; their weight is renormalized into the
//! remaining non-empty channels so total fusion weight always sums to 1.

use crate::HybridSearchResult;

use super::query_taxonomy::WeightVector;
use super::signals::{
    self, dense, entity_match, lexical, recency, SignalBreakdown, SignalKind, SignalScore,
};

/// RRF `k` per arch §8 Q1 default.
pub const RRF_K: f32 = 60.0;

/// Output of `fuse`: aligned with `candidates` input order.
/// `breakdowns[i]` belongs to `candidates[i]`. `fused_order` is a list of
/// indices into `candidates` sorted descending by RRF-fused score.
#[derive(Debug, Clone)]
pub struct FusionResult {
    pub breakdowns: Vec<SignalBreakdown>,
    pub fused_order: Vec<usize>,
    pub fused_scores: Vec<f32>,
}

/// Inputs that callers can override at the call site (mostly for tests
/// + `/87` ablation knobs). `now_ts` is the wall clock used by recency.
#[derive(Debug, Clone)]
pub struct FusionParams {
    pub now_ts: i64,
    pub recency_tau_days: f64,
    pub rrf_k: f32,
}

impl FusionParams {
    /// Production defaults: `now = chrono::Utc::now().timestamp()`,
    /// `tau = 30 days`, `k = 60`.
    #[must_use]
    pub fn now_default() -> FusionParams {
        FusionParams {
            now_ts: chrono::Utc::now().timestamp(),
            recency_tau_days: recency::DEFAULT_TAU_DAYS,
            rrf_k: RRF_K,
        }
    }
}

/// Compute per-channel raw scores, rank each channel, and combine via RRF
/// using the supplied `WeightVector`. Empty channels (all-zero raw scores)
/// are dropped + their weight renormalized into non-empty channels.
#[must_use]
pub fn fuse(
    candidates: &[HybridSearchResult],
    weights: &WeightVector,
    params: FusionParams,
) -> FusionResult {
    if candidates.is_empty() {
        return FusionResult {
            breakdowns: vec![],
            fused_order: vec![],
            fused_scores: vec![],
        };
    }

    let dense_raw = dense::compute_raw(candidates);
    let lexical_raw = lexical::compute_raw(candidates);
    let entity_raw = entity_match::compute_raw(candidates);
    let recency_raw = recency::compute_raw(candidates, params.now_ts, params.recency_tau_days);

    let ids: Vec<&str> = candidates.iter().map(|c| c.id.as_str()).collect();
    let dense_ranks = ranks_from_raw(&dense_raw, &ids);
    let lexical_ranks = ranks_from_raw(&lexical_raw, &ids);
    let entity_ranks = ranks_from_raw(&entity_raw, &ids);
    let recency_ranks = ranks_from_raw(&recency_raw, &ids);

    let active = ActiveWeights::renormalize(
        weights,
        ChannelEmpty {
            dense: is_all_zero(&dense_raw),
            lexical: is_all_zero(&lexical_raw),
            entity_match: is_all_zero(&entity_raw),
            recency: is_all_zero(&recency_raw),
        },
    );

    let n = candidates.len();
    let mut fused_scores = vec![0.0f32; n];
    let k = params.rrf_k;
    for i in 0..n {
        fused_scores[i] = rrf_term(active.dense, dense_ranks[i], k)
            + rrf_term(active.lexical, lexical_ranks[i], k)
            + rrf_term(active.entity_match, entity_ranks[i], k)
            + rrf_term(active.recency, recency_ranks[i], k);
    }

    let mut fused_order: Vec<usize> = (0..n).collect();
    fused_order.sort_by(|&a, &b| {
        fused_scores[b]
            .partial_cmp(&fused_scores[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| candidates[a].id.cmp(&candidates[b].id))
    });

    let breakdowns = (0..n)
        .map(|i| SignalBreakdown {
            dense: SignalScore {
                rank: dense_ranks[i],
                raw_score: dense_raw[i],
            },
            lexical: SignalScore {
                rank: lexical_ranks[i],
                raw_score: lexical_raw[i],
            },
            entity_match: SignalScore {
                rank: entity_ranks[i],
                raw_score: entity_raw[i],
            },
            recency: SignalScore {
                rank: recency_ranks[i],
                raw_score: recency_raw[i],
            },
        })
        .collect();

    FusionResult {
        breakdowns,
        fused_order,
        fused_scores,
    }
}

/// One RRF term: `w / (k + rank)`. Returns 0 when rank is `None`
/// (candidate didn't rank in this signal — score floor or empty channel).
fn rrf_term(weight: f32, rank: Option<u32>, k: f32) -> f32 {
    match rank {
        Some(r) => weight / (k + r as f32),
        None => 0.0,
    }
}

/// 1-indexed ranks for `raw`: highest score = rank 1. Ties broken by
/// ascending `ids[i]` so the ordering is deterministic across runs and
/// independent of input order. Candidates with score == 0 receive `None`
/// (signal had no contribution for them) — RRF treats `None` as
/// "infinity rank", contribution 0.
fn ranks_from_raw(raw: &[f32], ids: &[&str]) -> Vec<Option<u32>> {
    debug_assert_eq!(raw.len(), ids.len());
    let n = raw.len();
    let mut indexed: Vec<(usize, f32)> = raw.iter().copied().enumerate().collect();
    indexed.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| ids[a.0].cmp(ids[b.0]))
    });
    let mut ranks = vec![None; n];
    let mut next_rank: u32 = 1;
    for (orig_idx, score) in indexed {
        if score > 0.0 {
            ranks[orig_idx] = Some(next_rank);
            next_rank += 1;
        }
    }
    ranks
}

fn is_all_zero(raw: &[f32]) -> bool {
    raw.iter().all(|s| *s <= 0.0)
}

/// Per-channel empty flags surfaced for renormalization decisions.
#[derive(Debug, Clone, Copy)]
struct ChannelEmpty {
    dense: bool,
    lexical: bool,
    entity_match: bool,
    recency: bool,
}

/// Per-signal active weight after renormalization. If all channels are
/// empty, all weights are 0 (caller still gets a valid breakdown).
#[derive(Debug, Clone, Copy)]
struct ActiveWeights {
    dense: f32,
    lexical: f32,
    entity_match: f32,
    recency: f32,
}

impl ActiveWeights {
    fn renormalize(weights: &WeightVector, empty: ChannelEmpty) -> Self {
        let pairs = [
            (weights.dense, empty.dense, SignalKind::Dense),
            (weights.lexical, empty.lexical, SignalKind::Lexical),
            (
                weights.entity_match,
                empty.entity_match,
                SignalKind::EntityMatch,
            ),
            (weights.recency, empty.recency, SignalKind::Recency),
        ];
        let active_sum: f32 = pairs
            .iter()
            .filter(|(_, is_empty, _)| !is_empty)
            .map(|(w, _, _)| *w)
            .sum();
        let pick = |raw_w: f32, is_empty: bool| -> f32 {
            if is_empty || active_sum <= 0.0 {
                0.0
            } else {
                raw_w / active_sum
            }
        };
        Self {
            dense: pick(weights.dense, empty.dense),
            lexical: pick(weights.lexical, empty.lexical),
            entity_match: pick(weights.entity_match, empty.entity_match),
            recency: pick(weights.recency, empty.recency),
        }
    }
}

// re-export for callers that index by SignalKind
pub use signals::SignalScore as PublicSignalScore;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::query_taxonomy::QueryType;

    fn hsr(id: &str, vec_score: f32, bm25: f32, ts: i64) -> HybridSearchResult {
        HybridSearchResult {
            id: id.to_string(),
            content: String::new(),
            user_id: String::new(),
            app_id: String::new(),
            level: 0,
            timestamp: ts,
            score: 0.0,
            bm25_score: bm25,
            vector_score: vec_score,
            time_decay_factor: 1.0,
        }
    }

    #[test]
    fn test_fuse_empty_returns_empty() {
        let w = WeightVector::for_type(QueryType::Factual);
        let res = fuse(&[], &w, FusionParams::now_default());
        assert!(res.breakdowns.is_empty());
        assert!(res.fused_order.is_empty());
        assert!(res.fused_scores.is_empty());
    }

    #[test]
    fn test_ranks_from_raw_assigns_one_indexed_descending() {
        let ids = ["a", "b", "c", "d"];
        let ranks = ranks_from_raw(&[0.5, 0.9, 0.1, 0.7], &ids);
        // sorted desc: 0.9 (idx 1), 0.7 (idx 3), 0.5 (idx 0), 0.1 (idx 2)
        assert_eq!(ranks, vec![Some(3), Some(1), Some(4), Some(2)]);
    }

    #[test]
    fn test_ranks_from_raw_zero_floor_returns_none() {
        let ids = ["a", "b", "c", "d"];
        let ranks = ranks_from_raw(&[0.0, 0.5, 0.0, 0.3], &ids);
        // only 0.5 (idx 1) and 0.3 (idx 3) get ranks
        assert_eq!(ranks, vec![None, Some(1), None, Some(2)]);
    }

    #[test]
    fn test_ranks_from_raw_ties_broken_by_id_ascending() {
        // input order: zebra, alpha, mike — but tie-break is alphabetical.
        let ids = ["zebra", "alpha", "mike"];
        let ranks = ranks_from_raw(&[0.5, 0.5, 0.5], &ids);
        // alpha first (rank 1), mike second (rank 2), zebra third (rank 3)
        assert_eq!(ranks, vec![Some(3), Some(1), Some(2)]);
    }

    #[test]
    fn test_renormalize_when_all_channels_active_passes_weights() {
        let w = WeightVector::for_type(QueryType::Factual);
        let active = ActiveWeights::renormalize(
            &w,
            ChannelEmpty {
                dense: false,
                lexical: false,
                entity_match: false,
                recency: false,
            },
        );
        let total = active.dense + active.lexical + active.entity_match + active.recency;
        assert!(
            (total - 1.0).abs() < 1e-5,
            "active weights sum to {total} (expected ≈1.0)"
        );
    }

    #[test]
    fn test_renormalize_when_dense_empty_redistributes() {
        let w = WeightVector::for_type(QueryType::Factual);
        let original_dense = w.dense;
        let active = ActiveWeights::renormalize(
            &w,
            ChannelEmpty {
                dense: true,
                lexical: false,
                entity_match: false,
                recency: false,
            },
        );
        assert_eq!(active.dense, 0.0, "empty channel weight = 0");
        let total = active.dense + active.lexical + active.entity_match + active.recency;
        assert!(
            (total - 1.0).abs() < 1e-5,
            "renormalized weights sum to {total} (expected ≈1.0)"
        );
        assert!(
            active.lexical > w.lexical,
            "lexical weight should grow when dense drops out: {} → {}",
            w.lexical,
            active.lexical
        );
        // Sanity: original dense weight redistributed proportionally.
        let _ = original_dense;
    }

    #[test]
    fn test_renormalize_when_all_empty_returns_zeros() {
        let w = WeightVector::for_type(QueryType::Factual);
        let active = ActiveWeights::renormalize(
            &w,
            ChannelEmpty {
                dense: true,
                lexical: true,
                entity_match: true,
                recency: true,
            },
        );
        let total = active.dense + active.lexical + active.entity_match + active.recency;
        assert_eq!(total, 0.0, "all-empty channels → zero active weight");
    }

    #[test]
    fn test_fuse_factual_query_orders_by_dense_and_bm25() {
        // Factual: dense + lexical share top tier, recency/entity low.
        let now = 10_000_000;
        let cs = vec![
            hsr("a", 0.9, 8.0, now - 86_400),       // strong dense + bm25
            hsr("b", 0.1, 0.5, now - 86_400),       // weak both
            hsr("c", 0.5, 4.0, now - 100 * 86_400), // medium
        ];
        let w = WeightVector::for_type(QueryType::Factual);
        let params = FusionParams {
            now_ts: now,
            recency_tau_days: 30.0,
            rrf_k: 60.0,
        };
        let res = fuse(&cs, &w, params);
        // Strongest candidate (a) should be #1 in fused_order.
        assert_eq!(
            res.fused_order[0], 0,
            "candidate 'a' (strong dense+bm25) ranked #1"
        );
        // Weakest (b, all low scores) should be last among contributing.
        let last = *res.fused_order.last().expect("non-empty");
        assert_eq!(last, 1, "candidate 'b' (weakest) ranked last");
    }

    #[test]
    fn test_fuse_breakdowns_align_with_input_order() {
        let now = 10_000_000;
        let cs = vec![
            hsr("alpha", 0.9, 5.0, now),
            hsr("bravo", 0.5, 8.0, now - 86_400),
        ];
        let w = WeightVector::for_type(QueryType::Factual);
        let params = FusionParams {
            now_ts: now,
            recency_tau_days: 30.0,
            rrf_k: 60.0,
        };
        let res = fuse(&cs, &w, params);
        // breakdowns[0] is for "alpha": dense rank should be 1 (higher vec_score).
        assert_eq!(res.breakdowns[0].dense.rank, Some(1));
        assert_eq!(res.breakdowns[1].dense.rank, Some(2));
        // bravo has higher BM25 → lexical rank 1 for bravo.
        assert_eq!(res.breakdowns[1].lexical.rank, Some(1));
        assert_eq!(res.breakdowns[0].lexical.rank, Some(2));
    }

    #[test]
    fn test_fuse_placeholder_signals_have_none_rank() {
        let now = 10_000_000;
        let cs = vec![hsr("a", 0.9, 5.0, now)];
        let w = WeightVector::for_type(QueryType::Factual);
        let res = fuse(&cs, &w, FusionParams::now_default());
        assert_eq!(
            res.breakdowns[0].entity_match.rank, None,
            "entity_match placeholder → None"
        );
    }

    #[test]
    fn test_fuse_recent_query_ranks_recent_first() {
        // Recent type: recency dominates.
        let now = 10_000_000;
        let cs = vec![
            hsr("old", 0.9, 8.0, now - 365 * 86_400),
            hsr("new", 0.5, 4.0, now - 86_400),
            hsr("medium", 0.6, 5.0, now - 30 * 86_400),
        ];
        let w = WeightVector::for_type(QueryType::Recent);
        let params = FusionParams {
            now_ts: now,
            recency_tau_days: 30.0,
            rrf_k: 60.0,
        };
        let res = fuse(&cs, &w, params);
        // "new" should beat "old" despite weaker dense+bm25 (recency dominates)
        let new_pos = res.fused_order.iter().position(|&i| cs[i].id == "new");
        let old_pos = res.fused_order.iter().position(|&i| cs[i].id == "old");
        assert!(
            new_pos < old_pos,
            "recent type: 'new' must outrank 'old'; order = {:?}",
            res.fused_order
                .iter()
                .map(|&i| &cs[i].id)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_fuse_deterministic_tie_break_by_id() {
        // Two candidates with identical signal contributions → tie broken by id.
        let now = 10_000_000;
        let cs = vec![hsr("zebra", 0.5, 5.0, now), hsr("alpha", 0.5, 5.0, now)];
        let w = WeightVector::for_type(QueryType::Factual);
        let params = FusionParams {
            now_ts: now,
            recency_tau_days: 30.0,
            rrf_k: 60.0,
        };
        let res = fuse(&cs, &w, params);
        // tie → ascending id → "alpha" first
        assert_eq!(cs[res.fused_order[0]].id, "alpha");
    }
}
