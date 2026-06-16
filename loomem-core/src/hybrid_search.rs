use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::debug;

use crate::config::Config;
use crate::decay::DecayConfig;
use crate::graph::GraphSearchConfig;
use crate::tantivy_index::SearchResult;
use crate::vector_search;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchConfig {
    pub hybrid_weights: HybridWeightsConfig,
    pub decay: DecayConfig,
    pub surprise_boost: f64,
    pub top_k: usize,
    pub synonyms_file: String,
    pub stem_polish: bool,
    pub entities_file: String,
    #[serde(default)]
    pub rerank_enabled: bool,
    #[serde(default = "default_rerank_candidates")]
    pub rerank_candidates: usize,
    #[serde(default)]
    pub rerank_model_dir: Option<String>,
    #[serde(default)]
    pub multi_query_enabled: bool,
    /// Extend multi_query decomposition to vector search (not just BM25)
    #[serde(default)]
    pub vector_multi_query: bool,
    /// For counting/aggregation queries, prefer L0 raw chunks over L1 summaries
    #[serde(default)]
    pub counting_l0_preference: bool,
    #[serde(default)]
    pub importance: ImportanceConfig,
    #[serde(default)]
    pub cache: crate::query_cache::QueryCacheConfig,
    #[serde(default)]
    pub graph: GraphSearchConfig,
    #[serde(default)]
    pub complexity: ComplexityConfig,
    /// Cycle/118: log-frequency multiplier weight for access_count in
    /// final score. Default 0.0 (disabled, byte-identical to pre-cycle).
    /// Enable by setting to 0.3-0.5 in config.toml [search] section.
    #[serde(default = "default_implicit_access_boost_weight")]
    pub implicit_access_boost_weight: f64,
}

fn default_rerank_candidates() -> usize {
    10
}

fn default_implicit_access_boost_weight() -> f64 {
    0.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HybridWeightsConfig {
    pub vector: f64,
    pub bm25: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportanceConfig {
    pub high_weight: f64,
    pub medium_weight: f64,
    pub low_weight: f64,
    pub high_threshold: f64,
    pub low_threshold: f64,
}

impl Default for ImportanceConfig {
    fn default() -> Self {
        Self {
            high_weight: 1.5,
            medium_weight: 1.0,
            low_weight: 0.7,
            high_threshold: 0.5,
            low_threshold: 0.2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComplexityConfig {
    pub enabled: bool,
    pub simple_top_k: usize,
    pub medium_top_k: usize,
    pub complex_top_k: usize,
}

impl Default for ComplexityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            simple_top_k: 3,
            medium_top_k: 10,
            complex_top_k: 20,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HybridSearchResult {
    pub id: String,
    pub content: String,
    pub user_id: String,
    pub app_id: String,
    pub level: i32,
    pub timestamp: i64,
    pub score: f64,
    pub bm25_score: f32,
    pub vector_score: f32,
    pub time_decay_factor: f64,
}

pub struct HybridSearchEngine {
    config: Config,
}

impl HybridSearchEngine {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Compute time decay factor based on age and level-specific lambda
    fn compute_time_decay(&self, timestamp: i64, level: i32, decay_config: &DecayConfig) -> f64 {
        let now = Utc::now().timestamp();
        let age_seconds = (now - timestamp) as f64;

        // Select lambda based on level
        let lambda = match level {
            0 => decay_config.l0_lambda,
            _ => decay_config.l1_lambda,
        };

        // Exponential decay: e^(-lambda * age)
        // age is in seconds, so we normalize by dividing by 86400 (seconds per day)
        let age_days = age_seconds / 86400.0;
        let decay = (-lambda * age_days).exp();

        debug!(
            "Time decay: level={}, age_days={:.2}, lambda={}, decay={:.4}",
            level, age_days, lambda, decay
        );

        decay
    }

    /// Fuse BM25 and vector scores using vector_search results
    pub fn fuse_with_vector(
        &self,
        bm25_results: Vec<SearchResult>,
        vector_scores: Vec<(String, f32)>,
        store: Option<&crate::storage::RocksDbStore>,
    ) -> Result<Vec<HybridSearchResult>> {
        let weights = &self.config.search.hybrid_weights;
        let decay_config = &self.config.search.decay;

        // Normalize BM25 scores
        let max_bm25 = bm25_results
            .iter()
            .map(|r| r.score)
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(1.0);

        // Normalize vector scores (cosine similarity is typically -1 to 1, shift to 0-1)
        let max_vector = vector_scores
            .iter()
            .map(|r| r.1)
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(1.0);

        // Build maps for efficient lookup
        let mut bm25_map: HashMap<String, (f32, SearchResult)> = HashMap::new();
        for result in bm25_results {
            let normalized = result.score / max_bm25;
            bm25_map.insert(result.id.clone(), (normalized, result));
        }

        let mut vector_map: HashMap<String, f32> = HashMap::new();
        for (id, score) in vector_scores {
            let normalized = if max_vector > 0.0 {
                score / max_vector
            } else {
                0.0
            };
            vector_map.insert(id, normalized);
        }

        // Collect all unique IDs
        let mut all_ids: Vec<String> = bm25_map.keys().chain(vector_map.keys()).cloned().collect();
        all_ids.sort();
        all_ids.dedup();

        // Fuse scores
        let mut fused_results = Vec::new();
        for id in all_ids {
            let (bm25_score, doc) = match bm25_map.get(&id) {
                Some((score, doc)) => (*score, Some(doc)),
                None => (0.0, None),
            };
            let vector_score = *vector_map.get(&id).unwrap_or(&0.0);

            // Weighted fusion
            let fusion_score =
                weights.bm25 * bm25_score as f64 + weights.vector * vector_score as f64;

            // Get document details from BM25 results or fetch from store for vector-only hits
            let doc_data: Option<(String, String, String, i32, i64)> = if let Some(doc) = doc {
                Some((
                    doc.content.clone(),
                    doc.user_id.clone(),
                    doc.app_id.clone(),
                    doc.level,
                    doc.timestamp,
                ))
            } else if let Some(s) = store {
                // Vector-only hit — fetch from RocksDB
                if let Ok(Some(chunk)) = s.get_chunk(&id) {
                    Some((
                        chunk.content,
                        "default".to_string(),
                        "default".to_string(),
                        chunk.level,
                        chunk.timestamp as i64,
                    ))
                } else {
                    None
                }
            } else {
                None
            };

            if let Some((content, user_id, app_id, level, timestamp)) = doc_data {
                let time_decay = self.compute_time_decay(timestamp, level, decay_config);
                let final_score = fusion_score * time_decay;

                fused_results.push(HybridSearchResult {
                    id: id.clone(),
                    content,
                    user_id,
                    app_id,
                    level,
                    timestamp,
                    score: final_score,
                    bm25_score,
                    vector_score,
                    time_decay_factor: time_decay,
                });
            }
        }

        // Sort by final score descending
        fused_results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Keep a larger pool for downstream boosting/reranking (3x top_k)
        let pool_size = self.config.search.top_k * 3;
        fused_results.truncate(pool_size);

        debug!(
            "Fused {} results (BM25 + vector, pool={})",
            fused_results.len(),
            pool_size
        );

        Ok(fused_results)
    }

    /// Original fuse_scores for compatibility (converts SearchResult vector results)
    pub fn fuse_scores(
        &self,
        bm25_results: Vec<SearchResult>,
        vector_results: Vec<SearchResult>,
    ) -> Result<Vec<HybridSearchResult>> {
        // Convert SearchResult to (id, score) format
        let vector_scores: Vec<(String, f32)> = vector_results
            .into_iter()
            .map(|r| (r.id, r.score))
            .collect();

        self.fuse_with_vector(bm25_results, vector_scores, None)
    }

    /// Perform vector search and fuse with BM25
    /// stream_filter: if Some, post-filter vector results to only include chunks
    /// whose stream field matches one of the given streams (namespace isolation)
    pub fn search_with_vector(
        &self,
        bm25_results: Vec<SearchResult>,
        all_embeddings: &[(String, Vec<f32>)],
        query_embedding: &[f32],
        store: Option<&crate::storage::RocksDbStore>,
        stream_filter: Option<&[String]>,
    ) -> Result<Vec<HybridSearchResult>> {
        // Fetch extra candidates when stream filtering (some will be filtered out)
        let fetch_k = if stream_filter.is_some() {
            self.config.search.top_k * 4
        } else {
            self.config.search.top_k * 2
        };

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
        self.fuse_with_vector(bm25_results, vector_results, store)
    }

    /// Fuse BM25 and vector scores (legacy version for compatibility)
    #[allow(dead_code)]
    fn fuse_scores_legacy(
        &self,
        bm25_results: Vec<SearchResult>,
        vector_results: Vec<SearchResult>,
    ) -> Result<Vec<HybridSearchResult>> {
        let weights = &self.config.search.hybrid_weights;
        let decay_config = &self.config.search.decay;

        // Normalize BM25 scores
        let max_bm25 = bm25_results
            .iter()
            .map(|r| r.score)
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(1.0);

        // Normalize vector scores (cosine similarity is typically 0-1)
        let max_vector = vector_results
            .iter()
            .map(|r| r.score)
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(1.0);

        // Build maps for efficient lookup
        let mut bm25_map: HashMap<String, f32> = HashMap::new();
        for result in &bm25_results {
            bm25_map.insert(result.id.clone(), result.score / max_bm25);
        }

        let mut vector_map: HashMap<String, f32> = HashMap::new();
        for result in &vector_results {
            vector_map.insert(result.id.clone(), result.score / max_vector);
        }

        // Collect all unique IDs
        let mut all_ids: Vec<String> = bm25_results
            .iter()
            .map(|r| r.id.clone())
            .chain(vector_results.iter().map(|r| r.id.clone()))
            .collect();
        all_ids.sort();
        all_ids.dedup();

        // Build a map of full documents
        let mut doc_map: HashMap<String, SearchResult> = HashMap::new();
        for result in bm25_results.iter().chain(vector_results.iter()) {
            doc_map.insert(result.id.clone(), result.clone());
        }

        // Fuse scores
        let mut fused_results = Vec::new();
        for id in all_ids {
            let bm25_score = *bm25_map.get(&id).unwrap_or(&0.0);
            let vector_score = *vector_map.get(&id).unwrap_or(&0.0);

            // Weighted fusion
            let fusion_score =
                weights.bm25 * bm25_score as f64 + weights.vector * vector_score as f64;

            // Get document details
            if let Some(doc) = doc_map.get(&id) {
                // Apply time decay
                let time_decay = self.compute_time_decay(doc.timestamp, doc.level, decay_config);
                let final_score = fusion_score * time_decay;

                fused_results.push(HybridSearchResult {
                    id: doc.id.clone(),
                    content: doc.content.clone(),
                    user_id: doc.user_id.clone(),
                    app_id: doc.app_id.clone(),
                    level: doc.level,
                    timestamp: doc.timestamp,
                    score: final_score,
                    bm25_score,
                    vector_score,
                    time_decay_factor: time_decay,
                });
            }
        }

        // Sort by final score descending
        fused_results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Keep larger pool for downstream boosting/reranking
        let pool_size = self.config.search.top_k * 3;
        fused_results.truncate(pool_size);

        debug!("Fused {} results (pool={})", fused_results.len(), pool_size);

        Ok(fused_results)
    }

    /// Simple BM25-only search (for Phase 2, before vector embeddings are ready)
    pub fn bm25_only(&self, bm25_results: Vec<SearchResult>) -> Result<Vec<HybridSearchResult>> {
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
            let time_decay = self.compute_time_decay(doc.timestamp, doc.level, decay_config);
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

        // Keep larger pool for downstream boosting/reranking
        let pool_size = self.config.search.top_k * 3;
        results.truncate(pool_size);

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
            storage: crate::config::StorageConfig {
                data_dir: "./data".into(),
                rocksdb: crate::config::RocksDbConfig {
                    max_open_files: 100,
                    compression: "none".into(),
                    write_buffer_size: 4 * 1024 * 1024,
                    max_write_buffer_number: 2,
                },
                tantivy: crate::config::TantivyConfig {
                    enabled: true,
                    heap_size_mb: 128,
                    drift_warn_pct: 5.0,
                    auto_rebuild_on_drift: false,
                },
                vector_enabled: true,
                intent_log: crate::config::IntentLogConfig::default(),
            },
            search: crate::config::SearchConfig {
                top_k: 10,
                surprise_boost: 1.5,
                hybrid_weights: crate::config::HybridWeightsConfig {
                    vector: 0.6,
                    bm25: 0.4,
                },
                decay: crate::config::DecayConfig {
                    l0_lambda: 0.10,
                    l1_lambda: 0.03,
                },
                synonyms_file: "synonyms.toml".to_string(),
                entities_file: "entities.toml".to_string(),
                stem_polish: true,
                rerank_enabled: false,
                rerank_candidates: 10,
                rerank_model_dir: None,
                multi_query_enabled: false,
                vector_multi_query: false,
                counting_l0_preference: false,
                importance: crate::config::ImportanceConfig::default(),
                cache: crate::query_cache::QueryCacheConfig::default(),
                graph: crate::config::GraphSearchConfig::default(),
                complexity: crate::config::ComplexityConfig::default(),
                implicit_access_boost_weight: 0.0,
            },
            advisor: crate::config::AdvisorConfig::default(),
            worker: crate::config::WorkerConfig::default(),
            scheduler: crate::config::SchedulerConfig { enabled: false },
            llm: crate::config::LlmConfig::default(),
            server: crate::config::ServerConfig {
                host: "127.0.0.1".into(),
                port: 3030,
                auth_token_env: String::new(),
            },
            resource_guards: crate::config::ResourceGuardsConfig::default(),
            streams: crate::config::StreamsConfig::default(),
            namespaces: std::collections::HashMap::new(),
            pii: crate::config::PiiConfig::default(),
            cost: crate::config::CostConfig::default(),
            memory_generator: crate::config::MemoryGeneratorConfig::default(),
            entity_extraction: crate::config::EntityExtractionConfig::default(),
            contradiction: crate::config::ContradictionConfig::default(),
            knowledge_extraction: crate::config::KnowledgeExtractionConfig::default(),
            profile: crate::config::ProfileConfig::default(),
            manifest: crate::config::ManifestConfig::default(),
            dream: crate::config::DreamConfig::default(),

            retention: crate::config::RetentionConfig::default(),
            event_log: crate::config::EventLogConfig::default(),
            associator: crate::config::AssociatorConfig::default(),
            feedback: crate::config::FeedbackConfig::default(),
            content_type: crate::config::ContentTypeConfig::default(),
            access_audit: crate::config::AccessAuditConfig::default(),
        }
    }

    #[test]
    fn test_time_decay() {
        let config = test_config();
        let engine = HybridSearchEngine::new(config.clone());

        let now = Utc::now().timestamp();

        // Recent document (1 day old) at L0 - fast decay
        let decay_l0_1d = engine.compute_time_decay(now - 86400, 0, &config.search.decay);
        assert!(decay_l0_1d < 1.0);
        assert!(decay_l0_1d > 0.9); // L0 lambda=0.10, so e^(-0.1) ≈ 0.905
    }
}
