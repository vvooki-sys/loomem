//! Entity-match signal — placeholder in `/86` (Path A scope).
//!
//! Full implementation requires per-candidate entity-tag fetch from
//! RocksDB (`get_entities(chunk_id)`) and Jaccard / coverage computation
//! against `ParsedFeatures.entities` from the query classifier.
//!
//! Adding hot-path RocksDB reads to the search request flow is a separate
//! decision tracked as a follow-up cycle (see `/86 close.md` § Follow-ups).
//! Until then, this signal contributes 0 to RRF — `WeightVector` row sums
//! still respect renormalization on empty channels.

use crate::HybridSearchResult;

/// Placeholder: returns `0.0` for every candidate. Documented limitation —
/// full entity-tag matching is deferred to a follow-up cycle.
pub fn compute_raw(candidates: &[HybridSearchResult]) -> Vec<f32> {
    vec![0.0; candidates.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hsr(id: &str) -> HybridSearchResult {
        HybridSearchResult {
            id: id.to_string(),
            content: String::new(),
            user_id: String::new(),
            app_id: String::new(),
            level: 0,
            timestamp: 0,
            score: 0.0,
            bm25_score: 0.0,
            vector_score: 0.0,
            time_decay_factor: 1.0,
        }
    }

    #[test]
    fn test_entity_match_placeholder_returns_zeros() {
        let cs = vec![hsr("a"), hsr("b"), hsr("c")];
        let scores = compute_raw(&cs);
        assert_eq!(scores, vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn test_entity_match_empty() {
        assert!(compute_raw(&[]).is_empty());
    }
}
