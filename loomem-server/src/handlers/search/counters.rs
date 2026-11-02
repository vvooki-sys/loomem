//! Process-local retrieval counters: which path a search took and whether a
//! reranker ran. They never change responses; Hosted reports them per scope.
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
pub struct RetrievalCounters {
    /// Primary retrieval fused vector similarity with BM25.
    vector_searches: AtomicU64,
    /// Primary retrieval returned BM25 only (no embeddings or simple query).
    lexical_only_searches: AtomicU64,
    /// Asynchronous reranks that were started.
    rerank_invocations: AtomicU64,
    /// Asynchronous reranks whose model call returned an ordering.
    rerank_completions: AtomicU64,
}

impl RetrievalCounters {
    pub(super) fn record_vector_search(&self) {
        self.vector_searches.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_lexical_only_search(&self) {
        self.lexical_only_searches.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_rerank_invocation(&self) {
        self.rerank_invocations.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_rerank_completion(&self) {
        self.rerank_completions.fetch_add(1, Ordering::Relaxed);
    }

    /// Point-in-time copy for diagnostics.
    pub fn snapshot(&self) -> RetrievalSnapshot {
        RetrievalSnapshot {
            vector_searches: self.vector_searches.load(Ordering::Relaxed),
            lexical_only_searches: self.lexical_only_searches.load(Ordering::Relaxed),
            rerank_invocations: self.rerank_invocations.load(Ordering::Relaxed),
            rerank_completions: self.rerank_completions.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub struct RetrievalSnapshot {
    pub vector_searches: u64,
    pub lexical_only_searches: u64,
    pub rerank_invocations: u64,
    pub rerank_completions: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reflects_each_counter_independently() {
        let counters = RetrievalCounters::default();
        counters.record_vector_search();
        counters.record_vector_search();
        counters.record_lexical_only_search();
        counters.record_rerank_invocation();
        assert_eq!(
            counters.snapshot(),
            RetrievalSnapshot {
                vector_searches: 2,
                lexical_only_searches: 1,
                rerank_invocations: 1,
                rerank_completions: 0,
            }
        );
    }
}
