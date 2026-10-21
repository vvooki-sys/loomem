//! Per-request retrieval pool limits; legacy entry points retain configured limits.
use super::{relieve_decay, retain_pool_with_guaranteed, HybridSearchEngine, HybridSearchResult};
use crate::{storage::RocksDbStore, tantivy_index::SearchResult, vector_search};
use anyhow::{Context, Result};
use std::collections::HashSet;
use tracing::debug;

pub(super) fn default_max_request_top_k() -> usize {
    1000
}

/// Candidate counts, not final response count. Small requests retain the default
/// overfetch pool; larger requests expand both dense fetch and fusion retention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchCandidateBudget {
    dense: usize,
    scoped_dense: usize,
    fused: usize,
}

impl SearchCandidateBudget {
    /// The HTTP boundary owns request ceilings; the domain rejects arithmetic
    /// overflow rather than wrapping to a smaller, misleading candidate pool.
    pub fn new(configured_top_k: usize, requested_top_k: usize) -> Result<Self> {
        let effective = configured_top_k.max(requested_top_k);
        Ok(Self {
            dense: effective
                .checked_mul(2)
                .context("dense candidate budget overflow")?,
            scoped_dense: effective
                .checked_mul(4)
                .context("scoped candidate budget overflow")?,
            fused: effective
                .checked_mul(3)
                .context("fusion candidate budget overflow")?,
        })
    }

    pub fn vector_fetch(self, stream_filtered: bool) -> usize {
        if stream_filtered {
            self.scoped_dense
        } else {
            self.dense
        }
    }

    pub fn pool_size(self) -> usize {
        self.fused
    }
}

pub struct VectorSearchOptions<'a> {
    pub store: Option<&'a RocksDbStore>,
    pub stream_filter: Option<&'a [String]>,
    pub guaranteed: Option<&'a HashSet<String>>,
    pub budget: SearchCandidateBudget,
}

impl HybridSearchEngine {
    fn default_candidate_budget(&self) -> Result<SearchCandidateBudget> {
        SearchCandidateBudget::new(self.config.search.top_k, self.config.search.top_k)
    }

    pub fn fuse_with_vector_guaranteed(
        &self,
        bm25_results: Vec<SearchResult>,
        vector_scores: Vec<(String, f32)>,
        store: Option<&RocksDbStore>,
        guaranteed: Option<&HashSet<String>>,
    ) -> Result<Vec<HybridSearchResult>> {
        self.fuse_with_vector_budget(
            bm25_results,
            vector_scores,
            store,
            guaranteed,
            self.default_candidate_budget()?,
        )
    }

    pub fn search_with_vector_guaranteed(
        &self,
        bm25_results: Vec<SearchResult>,
        all_embeddings: &[(String, Vec<f32>)],
        query_embedding: &[f32],
        store: Option<&RocksDbStore>,
        stream_filter: Option<&[String]>,
        guaranteed: Option<&HashSet<String>>,
    ) -> Result<Vec<HybridSearchResult>> {
        self.search_with_vector_budget_inner(
            bm25_results,
            all_embeddings,
            query_embedding,
            VectorSearchOptions {
                store,
                stream_filter,
                guaranteed,
                budget: self.default_candidate_budget()?,
            },
        )
    }

    pub fn bm25_only_guaranteed(
        &self,
        bm25_results: Vec<SearchResult>,
        guaranteed: Option<&HashSet<String>>,
    ) -> Result<Vec<HybridSearchResult>> {
        self.bm25_only_with_budget(bm25_results, guaranteed, self.default_candidate_budget()?)
    }

    /// Explicit scoped retrieval requires storage to verify vector membership.
    pub fn search_with_vector_budget(
        &self,
        bm25_results: Vec<SearchResult>,
        all_embeddings: &[(String, Vec<f32>)],
        query_embedding: &[f32],
        options: VectorSearchOptions<'_>,
    ) -> Result<Vec<HybridSearchResult>> {
        anyhow::ensure!(
            options.stream_filter.is_none() || options.store.is_some(),
            "stream-filtered vector retrieval requires storage"
        );
        self.search_with_vector_budget_inner(bm25_results, all_embeddings, query_embedding, options)
    }

    fn search_with_vector_budget_inner(
        &self,
        bm25_results: Vec<SearchResult>,
        all_embeddings: &[(String, Vec<f32>)],
        query_embedding: &[f32],
        options: VectorSearchOptions<'_>,
    ) -> Result<Vec<HybridSearchResult>> {
        let VectorSearchOptions {
            store,
            stream_filter,
            guaranteed,
            budget,
        } = options;
        let fetch_k = budget.vector_fetch(stream_filter.is_some());

        // Perform vector search
        let vector_results = vector_search::vector_search(all_embeddings, query_embedding, fetch_k);

        // Post-filter vector results by stream to enforce namespace isolation
        let vector_results = if let (Some(streams), Some(s)) = (stream_filter, store) {
            vector_results
                .into_iter()
                .filter(|(id, _)| {
                    match s.get_chunk(id) {
                        Ok(Some(chunk)) => streams.contains(&chunk.stream),
                        _ => false, // Can't verify stream -> exclude
                    }
                })
                .collect()
        } else {
            vector_results
        };

        debug!(
            "Vector search returned {} results (stream_filter={:?}), fusing with {} BM25 results",
            vector_results.len(),
            stream_filter,
            bm25_results.len()
        );

        // Fuse with BM25 results
        self.fuse_with_vector_budget(bm25_results, vector_results, store, guaranteed, budget)
    }

    pub fn bm25_only_with_budget(
        &self,
        bm25_results: Vec<SearchResult>,
        guaranteed: Option<&std::collections::HashSet<String>>,
        budget: SearchCandidateBudget,
    ) -> Result<Vec<HybridSearchResult>> {
        let decay_config = &self.config.search.decay;

        // Normalize BM25 scores
        let max_bm25 = bm25_results
            .iter()
            .map(|r| r.score)
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(1.0);

        let mut results = Vec::new();
        for doc in bm25_results {
            let normalized_score = doc.score / max_bm25;
            // WS-1(b): same match-quality decay relief as the fused path —
            // identity with the default `decay_match_relief == 0.0`.
            let time_decay = relieve_decay(
                self.compute_time_decay(doc.timestamp, doc.level, decay_config),
                f64::from(normalized_score),
                self.config.search.decay_match_relief,
            );
            let final_score = normalized_score as f64 * time_decay;

            results.push(HybridSearchResult {
                id: doc.id.clone(),
                content: doc.content.clone(),
                user_id: doc.user_id.clone(),
                app_id: doc.app_id.clone(),
                level: doc.level,
                timestamp: doc.timestamp,
                score: final_score,
                bm25_score: normalized_score,
                vector_score: 0.0,
                time_decay_factor: time_decay,
            });
        }

        // Sort by final score descending
        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Keep larger pool for downstream boosting/reranking.
        // Cycle/012: guaranteed ids survive the cut (rare-term lane).
        let pool_size = budget.pool_size();
        retain_pool_with_guaranteed(&mut results, pool_size, guaranteed);

        Ok(results)
    }
}

#[cfg(test)]
#[path = "candidate_budget_tests.rs"]
mod tests;
