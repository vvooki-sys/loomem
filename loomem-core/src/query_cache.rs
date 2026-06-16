use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::time::Instant;

use crate::hybrid_search::HybridSearchResult;

pub struct CacheEntry {
    pub results: Vec<HybridSearchResult>,
    inserted_at: Instant,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QueryCacheConfig {
    pub enabled: bool,
    pub max_entries: usize,
    pub ttl_secs: u64,
}

impl Default for QueryCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries: 500,
            ttl_secs: 300,
        }
    }
}

pub struct QueryCache {
    entries: HashMap<u64, CacheEntry>,
    access_order: Vec<u64>,
    config: QueryCacheConfig,
}

impl QueryCache {
    pub fn new(config: QueryCacheConfig) -> Self {
        Self {
            entries: HashMap::new(),
            access_order: Vec::new(),
            config,
        }
    }

    /// Compute cache key from query parameters.
    pub fn hash_query(
        query: &str,
        streams: Option<&[String]>,
        entity: Option<&str>,
        date_from: Option<&str>,
        date_to: Option<&str>,
        top_k: usize,
    ) -> u64 {
        Self::hash_query_with_source(
            query, streams, entity, date_from, date_to, top_k, None, None,
        )
    }

    pub fn hash_query_with_source(
        query: &str,
        streams: Option<&[String]>,
        entity: Option<&str>,
        date_from: Option<&str>,
        date_to: Option<&str>,
        top_k: usize,
        source_agent: Option<&str>,
        exclude_source_agents: Option<&[String]>,
    ) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        query.hash(&mut hasher);
        streams.hash(&mut hasher);
        entity.hash(&mut hasher);
        date_from.hash(&mut hasher);
        date_to.hash(&mut hasher);
        top_k.hash(&mut hasher);
        source_agent.hash(&mut hasher);
        exclude_source_agents.hash(&mut hasher);
        hasher.finish()
    }

    /// Look up cached results. Returns None if miss or expired.
    pub fn get(&mut self, key: u64) -> Option<&Vec<HybridSearchResult>> {
        // Check TTL first
        if let Some(entry) = self.entries.get(&key) {
            if entry.inserted_at.elapsed().as_secs() >= self.config.ttl_secs {
                self.entries.remove(&key);
                self.access_order.retain(|k| *k != key);
                return None;
            }
        } else {
            return None;
        }

        // Update LRU position
        self.access_order.retain(|k| *k != key);
        self.access_order.push(key);

        self.entries.get(&key).map(|e| &e.results)
    }

    /// Store results in cache. Evicts LRU entry if at capacity.
    pub fn insert(&mut self, key: u64, results: Vec<HybridSearchResult>) {
        // Evict if at capacity
        while self.entries.len() >= self.config.max_entries && !self.access_order.is_empty() {
            let oldest = self.access_order.remove(0);
            self.entries.remove(&oldest);
        }

        self.entries.insert(
            key,
            CacheEntry {
                results,
                inserted_at: Instant::now(),
            },
        );

        self.access_order.retain(|k| *k != key);
        self.access_order.push(key);
    }

    /// Clear all cached entries.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.access_order.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get reranked results if available, otherwise fall back to standard cache.
    /// Returns (is_reranked, results).
    pub fn get_best(&mut self, key: u64) -> Option<(bool, Vec<HybridSearchResult>)> {
        let reranked_key = key ^ Self::RERANKED_SALT;
        // Try reranked first
        if self.is_valid(reranked_key) {
            self.touch_lru(reranked_key);
            if let Some(entry) = self.entries.get(&reranked_key) {
                return Some((true, entry.results.clone()));
            }
        }
        // Fall back to standard
        if self.is_valid(key) {
            self.touch_lru(key);
            if let Some(entry) = self.entries.get(&key) {
                return Some((false, entry.results.clone()));
            }
        }
        None
    }

    fn is_valid(&mut self, key: u64) -> bool {
        if let Some(entry) = self.entries.get(&key) {
            if entry.inserted_at.elapsed().as_secs() >= self.config.ttl_secs {
                self.entries.remove(&key);
                self.access_order.retain(|k| *k != key);
                return false;
            }
            true
        } else {
            false
        }
    }

    fn touch_lru(&mut self, key: u64) {
        self.access_order.retain(|k| *k != key);
        self.access_order.push(key);
    }

    /// Store reranked results under a separate key with 2x TTL.
    pub fn insert_reranked(&mut self, key: u64, results: Vec<HybridSearchResult>) {
        let reranked_key = key ^ Self::RERANKED_SALT;

        // Evict if at capacity
        while self.entries.len() >= self.config.max_entries && !self.access_order.is_empty() {
            let oldest = self.access_order.remove(0);
            self.entries.remove(&oldest);
        }

        self.entries.insert(
            reranked_key,
            CacheEntry {
                results,
                inserted_at: Instant::now(),
            },
        );

        self.access_order.retain(|k| *k != reranked_key);
        self.access_order.push(reranked_key);
    }

    const RERANKED_SALT: u64 = 0xDEAD_BEEF_CAFE_BABE;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> QueryCacheConfig {
        QueryCacheConfig {
            enabled: true,
            max_entries: 3,
            ttl_secs: 60,
        }
    }

    fn mock_results(n: usize) -> Vec<HybridSearchResult> {
        (0..n)
            .map(|i| HybridSearchResult {
                id: format!("id-{}", i),
                content: format!("content-{}", i),
                user_id: "user".to_string(),
                app_id: "app".to_string(),
                level: 0,
                timestamp: 1000 + i as i64,
                score: 1.0 - (i as f64 * 0.1),
                bm25_score: 0.5,
                vector_score: 0.5,
                time_decay_factor: 0.99,
            })
            .collect()
    }

    #[test]
    fn test_insert_and_get() {
        let mut cache = QueryCache::new(test_config());
        let key = QueryCache::hash_query("test query", None, None, None, None, 10);
        let results = mock_results(3);

        cache.insert(key, results.clone());
        let cached = cache.get(key);
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().len(), 3);
    }

    #[test]
    fn test_ttl_expiration() {
        let config = QueryCacheConfig {
            enabled: true,
            max_entries: 10,
            ttl_secs: 0, // expire immediately
        };
        let mut cache = QueryCache::new(config);
        let key = QueryCache::hash_query("test", None, None, None, None, 10);

        cache.insert(key, mock_results(1));
        // TTL=0 means expired on next get
        let cached = cache.get(key);
        assert!(cached.is_none(), "Should be expired");
    }

    #[test]
    fn test_lru_eviction() {
        let mut cache = QueryCache::new(test_config()); // max_entries=3

        let k1 = QueryCache::hash_query("q1", None, None, None, None, 10);
        let k2 = QueryCache::hash_query("q2", None, None, None, None, 10);
        let k3 = QueryCache::hash_query("q3", None, None, None, None, 10);
        let k4 = QueryCache::hash_query("q4", None, None, None, None, 10);

        cache.insert(k1, mock_results(1));
        cache.insert(k2, mock_results(1));
        cache.insert(k3, mock_results(1));
        assert_eq!(cache.len(), 3);

        // Insert 4th — should evict k1 (oldest)
        cache.insert(k4, mock_results(1));
        assert_eq!(cache.len(), 3);
        assert!(cache.get(k1).is_none(), "k1 should be evicted");
        assert!(cache.get(k4).is_some(), "k4 should exist");
    }

    #[test]
    fn test_clear() {
        let mut cache = QueryCache::new(test_config());
        let key = QueryCache::hash_query("test", None, None, None, None, 10);
        cache.insert(key, mock_results(1));
        assert_eq!(cache.len(), 1);

        cache.clear();
        assert_eq!(cache.len(), 0);
        assert!(cache.get(key).is_none());
    }

    #[test]
    fn test_hash_determinism() {
        let h1 = QueryCache::hash_query("test", Some(&["100".to_string()]), None, None, None, 10);
        let h2 = QueryCache::hash_query("test", Some(&["100".to_string()]), None, None, None, 10);
        assert_eq!(h1, h2, "Same inputs should produce same hash");
    }

    #[test]
    fn test_hash_sensitivity() {
        let h1 = QueryCache::hash_query("test", Some(&["100".to_string()]), None, None, None, 10);
        let h2 = QueryCache::hash_query("test", Some(&["200".to_string()]), None, None, None, 10);
        assert_ne!(h1, h2, "Different stream should produce different hash");

        let h3 = QueryCache::hash_query("test", None, None, None, None, 10);
        let h4 = QueryCache::hash_query("test", None, None, None, None, 20);
        assert_ne!(h3, h4, "Different top_k should produce different hash");
    }

    #[test]
    fn test_get_best_prefers_reranked() {
        let mut cache = QueryCache::new(test_config());
        let key = QueryCache::hash_query("test", None, None, None, None, 10);

        // Insert standard results
        cache.insert(key, mock_results(3));

        // get_best returns standard (not reranked)
        let (is_reranked, results) = cache.get_best(key).unwrap();
        assert!(!is_reranked);
        assert_eq!(results.len(), 3);

        // Insert reranked (different order)
        let mut reranked = mock_results(2);
        reranked[0].id = "reranked-0".to_string();
        cache.insert_reranked(key, reranked);

        // get_best now returns reranked
        let (is_reranked, results) = cache.get_best(key).unwrap();
        assert!(is_reranked, "Should prefer reranked");
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "reranked-0");
    }
}
