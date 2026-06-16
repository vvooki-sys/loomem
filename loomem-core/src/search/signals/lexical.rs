//! Lexical (BM25) signal — re-emits the per-candidate BM25 score from the
//! existing Tantivy retrieval pass.

use crate::HybridSearchResult;

/// Per-candidate lexical score: pass-through of `HybridSearchResult.bm25_score`.
/// Range `[0, +∞)` (BM25 is unbounded above; typical values 0..15 for short
/// queries on chunk-sized documents). 0 for candidates that came purely from
/// vector search.
pub fn compute_raw(candidates: &[HybridSearchResult]) -> Vec<f32> {
    candidates.iter().map(|c| c.bm25_score).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hsr(id: &str, bm25_score: f32) -> HybridSearchResult {
        HybridSearchResult {
            id: id.to_string(),
            content: String::new(),
            user_id: String::new(),
            app_id: String::new(),
            level: 0,
            timestamp: 0,
            score: 0.0,
            bm25_score,
            vector_score: 0.0,
            time_decay_factor: 1.0,
        }
    }

    #[test]
    fn test_lexical_passthrough_bm25_score() {
        let cs = vec![hsr("a", 8.4), hsr("b", 0.0), hsr("c", 3.1)];
        let scores = compute_raw(&cs);
        assert_eq!(scores, vec![8.4, 0.0, 3.1]);
    }

    #[test]
    fn test_lexical_empty() {
        assert!(compute_raw(&[]).is_empty());
    }
}
