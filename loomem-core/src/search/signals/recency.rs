//! Recency signal — exponential decay against the candidate's
//! `decision_time` (= `HybridSearchResult.timestamp`, Unix seconds).

use crate::HybridSearchResult;

/// Default tau in days (per arch §5.1 — adjustable by env in `/87` ablation).
pub const DEFAULT_TAU_DAYS: f64 = 30.0;
const SECONDS_PER_DAY: f64 = 86_400.0;

/// Per-candidate recency: `exp(-Δt / τ)` where `Δt` = `now - timestamp`,
/// in days, and `τ` = `tau_days`.
///
/// Range `[0, 1]` (1 = same instant as `now_ts`, decays toward 0).
/// Negative `Δt` (timestamps in the future) clamp to `Δt = 0`.
pub fn compute_raw(candidates: &[HybridSearchResult], now_ts: i64, tau_days: f64) -> Vec<f32> {
    let tau_seconds = tau_days.max(1e-3) * SECONDS_PER_DAY;
    candidates
        .iter()
        .map(|c| {
            let delta = ((now_ts - c.timestamp).max(0) as f64) / tau_seconds;
            (-delta).exp() as f32
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hsr(id: &str, timestamp: i64) -> HybridSearchResult {
        HybridSearchResult {
            id: id.to_string(),
            content: String::new(),
            user_id: String::new(),
            app_id: String::new(),
            level: 0,
            timestamp,
            score: 0.0,
            bm25_score: 0.0,
            vector_score: 0.0,
            time_decay_factor: 1.0,
        }
    }

    #[test]
    fn test_recency_now_returns_one() {
        let now = 1_000_000;
        let cs = vec![hsr("a", now)];
        let scores = compute_raw(&cs, now, DEFAULT_TAU_DAYS);
        assert!((scores[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_recency_one_tau_returns_e_inverse() {
        let now = 1_000_000;
        let one_tau_seconds = (DEFAULT_TAU_DAYS * 86_400.0) as i64;
        let cs = vec![hsr("a", now - one_tau_seconds)];
        let scores = compute_raw(&cs, now, DEFAULT_TAU_DAYS);
        // exp(-1) ≈ 0.3679
        assert!(
            (scores[0] - (-1.0_f32).exp()).abs() < 1e-5,
            "expected e⁻¹ ≈ 0.368, got {}",
            scores[0]
        );
    }

    #[test]
    fn test_recency_far_past_decays_to_near_zero() {
        let now = 1_000_000_000;
        let cs = vec![hsr("a", 0)]; // ~30+ years ago
        let scores = compute_raw(&cs, now, DEFAULT_TAU_DAYS);
        assert!(
            scores[0] < 1e-6,
            "should decay to near-zero, got {}",
            scores[0]
        );
    }

    #[test]
    fn test_recency_future_timestamp_clamps_to_now() {
        // Candidate timestamp later than `now_ts` → Δt clamped to 0 → score = 1.
        let now = 1_000_000;
        let cs = vec![hsr("a", now + 999_999)];
        let scores = compute_raw(&cs, now, DEFAULT_TAU_DAYS);
        assert!((scores[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_recency_zero_tau_safe() {
        // Defensive: tau_days=0 must not divide by zero. Floor applied to 1e-3.
        let now = 1_000_000;
        let cs = vec![hsr("a", now - 86_400)];
        let scores = compute_raw(&cs, now, 0.0);
        assert!(scores[0].is_finite(), "score must not be NaN/Inf");
    }

    #[test]
    fn test_recency_ranks_newer_higher() {
        let now = 10_000_000;
        let cs = vec![
            hsr("oldest", now - 365 * 86_400),
            hsr("newest", now - 86_400),
            hsr("middle", now - 30 * 86_400),
        ];
        let scores = compute_raw(&cs, now, DEFAULT_TAU_DAYS);
        assert!(scores[1] > scores[2], "newest > middle");
        assert!(scores[2] > scores[0], "middle > oldest");
    }
}
