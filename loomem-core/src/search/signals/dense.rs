//! Dense (vector cosine) signal — re-emits the per-candidate cosine score
//! computed during the existing hybrid retrieval pass.

use crate::HybridSearchResult;

/// Per-candidate dense score: pass-through of `HybridSearchResult.vector_score`.
/// Range `[0, 1]` for cosine; 0 for candidates that came purely from BM25.
pub fn compute_raw(candidates: &[HybridSearchResult]) -> Vec<f32> {
    candidates.iter().map(|c| c.vector_score).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hsr(id: &str, vector_score: f32) -> HybridSearchResult {
        HybridSearchResult {
            id: id.to_string(),
            content: String::new(),
            user_id: String::new(),
            app_id: String::new(),
            level: 0,
            timestamp: 0,
            score: 0.0,
            bm25_score: 0.0,
            vector_score,
            time_decay_factor: 1.0,
        }
    }

    #[test]
    fn test_dense_passthrough_vector_score() {
        let cs = vec![hsr("a", 0.91), hsr("b", 0.12), hsr("c", 0.55)];
        let scores = compute_raw(&cs);
        assert_eq!(scores, vec![0.91, 0.12, 0.55]);
    }

    #[test]
    fn test_dense_empty() {
        assert!(compute_raw(&[]).is_empty());
    }
}
