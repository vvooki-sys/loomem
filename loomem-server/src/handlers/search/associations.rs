//! Authoritative visibility at the final association response boundary.
use loomem_core::associator::serendipity::ScoredAssociation;

use super::Association;
use crate::AppState;

pub(super) fn finalize(
    mut candidates: Vec<ScoredAssociation>,
    state: &AppState,
    stream: &str,
    limit: usize,
) -> Vec<ScoredAssociation> {
    candidates.retain(|candidate| {
        state
            .store
            .get_chunk(&candidate.chunk_id)
            .ok()
            .flatten()
            .is_some_and(|chunk| loomem_core::associator::is_associable_in_stream(&chunk, stream))
    });
    candidates.sort_by(|left, right| {
        right
            .serendipity_score
            .partial_cmp(&left.serendipity_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.dedup_by(|left, right| left.chunk_id == right.chunk_id);
    candidates.truncate(limit);
    candidates
}

pub(super) fn serialize(candidates: Vec<ScoredAssociation>) -> Vec<Association> {
    candidates
        .into_iter()
        .map(|candidate| Association {
            content: candidate.content,
            score: candidate.serendipity_score,
            source_mechanism: candidate.source_mechanism,
            explanation: Some(format!(
                "relevance={:.2} obviousness={:.2} cluster_dist={:.2}",
                candidate.relevance, candidate.obviousness, candidate.cluster_dist
            )),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::search::stream_scoping_e2e_tests::seed_one;

    fn candidate(id: &str, score: f64) -> ScoredAssociation {
        ScoredAssociation {
            chunk_id: id.into(),
            content: format!("stale candidate content {id}"),
            serendipity_score: score,
            relevance: 0.5,
            obviousness: 0.2,
            cluster_dist: 0.4,
            source_mechanism: "adjacent_possible".into(),
        }
    }

    #[tokio::test]
    async fn final_boundary_rejects_tombstoned_missing_and_cross_stream_before_limit() {
        let (_, state) = crate::tests::make_test_app();
        seed_one(
            &state,
            "deleted",
            "streamA",
            "synthetic deleted association",
            None,
        )
        .await;
        seed_one(
            &state,
            "outside",
            "streamB",
            "synthetic outside association",
            None,
        )
        .await;
        seed_one(
            &state,
            "alive",
            "streamA",
            "synthetic surviving association",
            None,
        )
        .await;
        state.store.delete_by_id("deleted").unwrap();
        // Index cleanup is intentionally absent: the final store check is authoritative.
        let candidates = vec![
            candidate("deleted", 1.0),
            candidate("outside", 0.9),
            candidate("missing", 0.8),
            candidate("alive", 0.2),
        ];
        let result = finalize(candidates, &state, "streamA", 1);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].chunk_id, "alive");
        let response = serialize(result);
        assert_eq!(response.len(), 1);
        assert!(!response[0].content.contains("deleted"));
        assert!(!response[0].content.contains("outside"));
    }
}
