use tracing::{debug, warn};

/// Count embeddings whose dimension differs from `query_dim`. Split out so
/// the mismatch alarm (audit 2026-07-01 item 7) is unit-testable.
fn count_dim_mismatches(embeddings: &[(String, Vec<f32>)], query_dim: usize) -> usize {
    embeddings
        .iter()
        .filter(|(_, e)| e.len() != query_dim)
        .count()
}

/// Compute cosine similarity between two vectors
/// Returns a value between -1.0 and 1.0 (higher is more similar)
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }

    let dot_product: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }

    dot_product / (norm_a * norm_b)
}

/// Perform full-scan vector search with cosine similarity
/// Returns top_k results sorted by similarity (highest first)
pub fn vector_search(
    embeddings: &[(String, Vec<f32>)],
    query_embedding: &[f32],
    top_k: usize,
) -> Vec<(String, f32)> {
    if embeddings.is_empty() || query_embedding.is_empty() {
        return Vec::new();
    }

    // Audit 2026-07-01 item 7: `cosine_similarity` scores mismatched
    // dimensions as 0.0, which silently degrades retrieval to BM25-only
    // (e.g. [llm].embedding_dim flipped between 384/local and 1536/openai
    // without re-embedding). Make the misconfiguration loud instead.
    let mismatched = count_dim_mismatches(embeddings, query_embedding.len());
    if mismatched > 0 {
        warn!(
            query_dim = query_embedding.len(),
            mismatched,
            total = embeddings.len(),
            "embedding dimension mismatch: {mismatched}/{} stored vectors will score 0.0 — \
             check [llm].embedding_dim against the stored index (run `loomem-server --reembed`)",
            embeddings.len()
        );
    }

    debug!(
        "Vector search: {} embeddings, query_dim={}, top_k={}",
        embeddings.len(),
        query_embedding.len(),
        top_k
    );

    // Compute similarity for all embeddings
    let mut scores: Vec<(String, f32)> = embeddings
        .iter()
        .map(|(id, embedding)| {
            let similarity = cosine_similarity(query_embedding, embedding);
            (id.clone(), similarity)
        })
        .collect();

    // Sort by similarity descending
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Take top_k
    scores.truncate(top_k);

    debug!("Vector search found {} results", scores.len());

    scores
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![-1.0, -2.0, -3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim + 1.0).abs() < 0.001);
    }

    #[test]
    fn test_vector_search() {
        let embeddings = vec![
            ("doc1".to_string(), vec![1.0, 0.0, 0.0]),
            ("doc2".to_string(), vec![0.0, 1.0, 0.0]),
            ("doc3".to_string(), vec![0.9, 0.1, 0.0]),
        ];

        let query = vec![1.0, 0.0, 0.0];
        let results = vector_search(&embeddings, &query, 2);

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "doc1"); // Exact match
        assert_eq!(results[1].0, "doc3"); // Close match
        assert!(results[0].1 > results[1].1); // Scores ordered correctly
    }

    #[test]
    fn test_vector_search_empty() {
        let embeddings = vec![];
        let query = vec![1.0, 0.0];
        let results = vector_search(&embeddings, &query, 10);
        assert_eq!(results.len(), 0);
    }

    // Audit 2026-07-01 item 7: dimension-mismatch counting.
    #[test]
    fn count_dim_mismatches_flags_wrong_dims() {
        let embeddings = vec![
            ("ok".to_string(), vec![1.0, 0.0, 0.0]),
            ("short".to_string(), vec![1.0, 0.0]),
            ("long".to_string(), vec![1.0, 0.0, 0.0, 0.0]),
        ];
        assert_eq!(count_dim_mismatches(&embeddings, 3), 2);
        assert_eq!(count_dim_mismatches(&embeddings, 2), 2);
    }

    #[test]
    fn count_dim_mismatches_zero_when_consistent() {
        let embeddings = vec![
            ("a".to_string(), vec![1.0, 0.0]),
            ("b".to_string(), vec![0.0, 1.0]),
        ];
        assert_eq!(count_dim_mismatches(&embeddings, 2), 0);
    }

    // Mismatched vectors keep scoring 0.0 (existing contract) — the warn!
    // added by audit item 7 must not change ranking behavior.
    #[test]
    fn mismatched_dims_score_zero_and_rank_last() {
        let embeddings = vec![
            ("match".to_string(), vec![1.0, 0.0, 0.0]),
            ("mismatch".to_string(), vec![1.0, 0.0]),
        ];
        let results = vector_search(&embeddings, &[1.0, 0.0, 0.0], 2);
        assert_eq!(results[0].0, "match");
        assert!(results[0].1 > 0.9);
        assert_eq!(results[1].0, "mismatch");
        assert_eq!(results[1].1, 0.0);
    }
}
