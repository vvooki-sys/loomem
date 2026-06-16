use tracing::debug;

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
}
