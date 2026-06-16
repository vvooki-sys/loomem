use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

use crate::auth::AuthContext;

/// Per-workspace rate limit state.
struct BucketState {
    tokens: f64,
    last_refill: Instant,
}

/// In-memory token-bucket rate limiter keyed by workspace (stream_id).
pub struct RateLimiter {
    buckets: Arc<RwLock<HashMap<String, HashMap<String, BucketState>>>>,
    config: RateLimitConfig,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RateLimitConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Max requests per minute for search operations.
    #[serde(default = "default_search_rpm")]
    pub search_rpm: u32,
    /// Max requests per minute for store/ingest operations.
    #[serde(default = "default_store_rpm")]
    pub store_rpm: u32,
    /// Max requests per minute for destructive operations (delete/purge).
    #[serde(default = "default_delete_rpm")]
    pub delete_rpm: u32,
}

fn default_enabled() -> bool {
    false
}
fn default_search_rpm() -> u32 {
    120
}
fn default_store_rpm() -> u32 {
    60
}
fn default_delete_rpm() -> u32 {
    10
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            search_rpm: 120,
            store_rpm: 60,
            delete_rpm: 10,
        }
    }
}

/// Operation category for rate limiting.
pub enum OpCategory {
    Search,
    Store,
    Delete,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            buckets: Arc::new(RwLock::new(HashMap::new())),
            config,
        }
    }

    /// Check if the operation is allowed. Returns Ok(()) or Err with retry-after seconds.
    pub async fn check(&self, auth: &AuthContext, op: OpCategory) -> Result<(), u64> {
        if !self.config.enabled || auth.is_admin {
            return Ok(());
        }

        let (rpm, category_key) = match op {
            OpCategory::Search => (self.config.search_rpm, "search"),
            OpCategory::Store => (self.config.store_rpm, "store"),
            OpCategory::Delete => (self.config.delete_rpm, "delete"),
        };

        let workspace = &auth.stream_id;
        let tokens_per_sec = rpm as f64 / 60.0;
        let max_tokens = rpm as f64; // burst = 1 minute worth

        let mut buckets = self.buckets.write().await;
        let workspace_buckets = buckets
            .entry(workspace.clone())
            .or_insert_with(HashMap::new);
        let bucket = workspace_buckets
            .entry(category_key.to_string())
            .or_insert_with(|| BucketState {
                tokens: max_tokens,
                last_refill: Instant::now(),
            });

        // Refill tokens based on elapsed time
        let elapsed = bucket.last_refill.elapsed().as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * tokens_per_sec).min(max_tokens);
        bucket.last_refill = Instant::now();

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            // Calculate retry-after: how long until 1 token is available
            let wait_secs = ((1.0 - bucket.tokens) / tokens_per_sec).ceil() as u64;
            Err(wait_secs.max(1))
        }
    }
}
