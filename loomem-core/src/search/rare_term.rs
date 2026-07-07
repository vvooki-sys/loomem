//! Rare-term guarantee lane (cycle/012).
//!
//! Production incident (2026-07-07): a fact about a rarely-mentioned entity,
//! buried as a single line inside a long profile chunk about a *different*
//! subject, is practically unfindable through `memory_search` — the fused
//! score (weighted fusion × time decay) ranks fresh, loosely-matching chunks
//! above the single strong lexical match.
//!
//! This module implements the *selection* half of the lane: deciding which
//! query tokens are "rare" (document frequency at or below a configured
//! threshold) so that chunks containing them can be force-included in the
//! candidate pool that feeds the reranker. Posting-list retrieval itself
//! lives in `TantivyIndex::term_candidates` (same index + field the BM25
//! channel uses — no separate content scan); pool-membership exemption lives
//! in `hybrid_search.rs`. The lane never bypasses the reranker and never
//! injects anything directly into final results — it only guarantees
//! presence in the candidate pool.
//!
//! Behind `[search.rare_term_lane] enabled` (default **off**): with the flag
//! off no lane code runs on the hot path and the execution path is identical
//! to pre-cycle behaviour.

use serde::{Deserialize, Serialize};

/// Sub-config for the rare-term guarantee lane. Composed into
/// `SearchConfig` as `rare_term_lane` (same pattern as `graph` / `cache`).
/// `#[serde(default)]` on every field keeps existing `config.toml` files and
/// persisted state deserializable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RareTermLaneConfig {
    /// Master switch. Default `false` — lane fully inert.
    #[serde(default)]
    pub enabled: bool,
    /// A token is rare when `df <= max(df_absolute_max, ceil(df_ratio_max * n_docs))`.
    #[serde(default = "default_df_absolute_max")]
    pub df_absolute_max: u64,
    /// Relative component of the rarity threshold (fraction of corpus size).
    #[serde(default = "default_df_ratio_max")]
    pub df_ratio_max: f64,
    /// Maximum number of mandatory candidates injected per query. When the
    /// posting lists yield more, the top `candidate_cap` by BM25 score win.
    #[serde(default = "default_candidate_cap")]
    pub candidate_cap: usize,
}

fn default_df_absolute_max() -> u64 {
    3
}

fn default_df_ratio_max() -> f64 {
    0.005
}

fn default_candidate_cap() -> usize {
    10
}

impl Default for RareTermLaneConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            df_absolute_max: default_df_absolute_max(),
            df_ratio_max: default_df_ratio_max(),
            candidate_cap: default_candidate_cap(),
        }
    }
}

/// A query token together with its document frequency in the index —
/// surfaced in channel diagnostics so per-query rarity decisions are
/// auditable.
#[derive(Debug, Clone, Serialize)]
pub struct RareToken {
    pub token: String,
    pub df: u64,
}

/// Rarity cutoff: `max(df_absolute_max, ceil(df_ratio_max * n_docs))`.
#[must_use]
pub fn rare_df_threshold(n_docs: u64, cfg: &RareTermLaneConfig) -> u64 {
    let ratio_cut = (cfg.df_ratio_max * clamped_f64(n_docs)).ceil();
    cfg.df_absolute_max.max(f64_to_u64_saturating(ratio_cut))
}

/// `u64 → f64` for the ratio product. Corpus sizes far below 2^32 in
/// practice; clamp through `u32` to stay in f64's exact-integer range.
fn clamped_f64(n: u64) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

/// Saturating `f64 → u64` for a non-negative, already-`ceil`ed threshold.
fn f64_to_u64_saturating(x: f64) -> u64 {
    if x.is_nan() || x <= 0.0 {
        0
    } else if x >= 9_007_199_254_740_992.0 {
        // 2^53 — beyond exact-integer f64 range; treat as unbounded.
        u64::MAX
    } else {
        // truncation intentional: x is non-negative, finite and below 2^53,
        // so the cast is exact for the integral value produced by `ceil()`.
        x as u64
    }
}

/// Select the rare tokens of a query. `tokens` are the query terms produced
/// by the *index* tokenizer for the same field the BM25 channel searches
/// (see `TantivyIndex::tokenize_content`), so `df` lookups agree with the
/// posting lists. Duplicates are dropped (first occurrence wins); tokens
/// with `df == 0` (absent from the corpus) are skipped — there is no
/// posting list to guarantee anything from.
///
/// `df_of` reports the document frequency of a token; errors abort the
/// selection so callers can degrade gracefully (lane off for this query).
pub fn select_rare_tokens<F>(
    tokens: &[String],
    threshold: u64,
    mut df_of: F,
) -> anyhow::Result<Vec<RareToken>>
where
    F: FnMut(&str) -> anyhow::Result<u64>,
{
    let mut seen = std::collections::HashSet::new();
    let mut rare = Vec::new();
    for token in tokens {
        if token.is_empty() || !seen.insert(token.as_str()) {
            continue;
        }
        let df = df_of(token)?;
        if df > 0 && df <= threshold {
            rare.push(RareToken {
                token: token.clone(),
                df,
            });
        }
    }
    Ok(rare)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RareTermLaneConfig {
        RareTermLaneConfig::default()
    }

    #[test]
    fn test_default_config_is_off_with_brief_start_values() {
        let c = cfg();
        assert!(!c.enabled);
        assert_eq!(c.df_absolute_max, 3);
        assert!((c.df_ratio_max - 0.005).abs() < 1e-12);
        assert_eq!(c.candidate_cap, 10);
    }

    #[test]
    fn test_threshold_absolute_floor_dominates_small_corpora() {
        // 200 docs * 0.005 = 1.0 → ceil 1 → max(3, 1) = 3.
        assert_eq!(rare_df_threshold(200, &cfg()), 3);
        assert_eq!(rare_df_threshold(0, &cfg()), 3);
    }

    #[test]
    fn test_threshold_ratio_takes_over_on_large_corpora() {
        // 10_000 docs * 0.005 = 50.
        assert_eq!(rare_df_threshold(10_000, &cfg()), 50);
        // 1_001 docs * 0.005 = 5.005 → ceil 6.
        assert_eq!(rare_df_threshold(1_001, &cfg()), 6);
    }

    #[test]
    fn test_select_rare_tokens_filters_by_threshold_and_dedupes() {
        let tokens = vec![
            "wrzosik".to_string(),
            "celina".to_string(),
            "wrzosik".to_string(), // duplicate — must not double-count
            "projekt".to_string(),
            "widmo".to_string(), // df 0 — absent, skipped
        ];
        let df = |t: &str| -> anyhow::Result<u64> {
            Ok(match t {
                "wrzosik" => 1,
                "celina" => 7,
                "projekt" => 180,
                _ => 0,
            })
        };
        let rare = select_rare_tokens(&tokens, 3, df).expect("df closure never fails");
        assert_eq!(rare.len(), 1);
        assert_eq!(rare[0].token, "wrzosik");
        assert_eq!(rare[0].df, 1);
    }

    #[test]
    fn test_select_rare_tokens_propagates_df_errors() {
        let tokens = vec!["boom".to_string()];
        let df = |_: &str| -> anyhow::Result<u64> { anyhow::bail!("index unavailable") };
        assert!(select_rare_tokens(&tokens, 3, df).is_err());
    }

    #[test]
    fn test_f64_to_u64_saturating_edges() {
        assert_eq!(f64_to_u64_saturating(f64::NAN), 0);
        assert_eq!(f64_to_u64_saturating(-1.0), 0);
        assert_eq!(f64_to_u64_saturating(0.0), 0);
        assert_eq!(f64_to_u64_saturating(6.0), 6);
        assert_eq!(f64_to_u64_saturating(f64::INFINITY), u64::MAX);
    }
}
