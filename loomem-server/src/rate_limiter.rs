use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

use axum::extract::Request;
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tracing::warn;

use crate::auth::AuthContext;
use loomem_core::config::RateLimitConfig;
use loomem_core::storage::DEFAULT_STREAM_ID;

/// Per-stream rate limit state.
struct BucketState {
    tokens: f64,
    last_refill: Instant,
}

/// In-memory token-bucket rate limiter keyed by stream id.
///
/// Wired in by the 2026-07-01 security audit (item 3) — it was dead code
/// before. Enforcement points: [`rate_limit_middleware`] on the `/v1`/`/api`
/// routes and the MCP dispatcher choke point (`dispatch_tool`).
///
/// There is deliberately NO admin exemption: in the single-user model every
/// caller authenticates as admin, so an exemption would make the limiter a
/// no-op. The realistic abuser is a runaway agent loop holding the one valid
/// key, and it must be throttled too.
pub struct RateLimiter {
    buckets: Arc<RwLock<HashMap<String, HashMap<String, BucketState>>>>,
    config: RateLimitConfig,
}

/// Operation category for rate limiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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

    /// Check whether one operation on `stream` is allowed right now.
    ///
    /// `Ok(())` consumes a token; `Err(retry_after_secs)` means the bucket is
    /// empty. Burst capacity = one minute's worth of tokens per category.
    /// Always `Ok` when `[rate_limit].enabled = false`.
    pub async fn check(&self, stream: &str, op: OpCategory) -> Result<(), u64> {
        if !self.config.enabled {
            return Ok(());
        }

        let (rpm, category_key) = match op {
            OpCategory::Search => (self.config.search_rpm, "search"),
            OpCategory::Store => (self.config.store_rpm, "store"),
            OpCategory::Delete => (self.config.delete_rpm, "delete"),
        };

        let tokens_per_sec = f64::from(rpm) / 60.0;
        let max_tokens = f64::from(rpm); // burst = 1 minute worth

        let mut buckets = self.buckets.write().await;
        let workspace_buckets = buckets.entry(stream.to_string()).or_default();
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
            // Calculate retry-after: how long until 1 token is available.
            // truncation intentional: ceil()ed small positive f64 → seconds
            let wait_secs = ((1.0 - bucket.tokens) / tokens_per_sec).ceil() as u64;
            Err(wait_secs.max(1))
        }
    }
}

/// Map an HTTP request to a rate-limit category. `None` = unlimited.
///
/// Only the hot, caller-facing paths are limited. Admin/maintenance endpoints
/// (re-embed, rebuilds, backfills, stats, workers) and the legacy
/// `PUT /api/memories/{id}` correction path are deliberately unlimited so
/// bulk operator jobs are never throttled.
pub fn categorize_http(method: &Method, path: &str) -> Option<OpCategory> {
    if method == Method::DELETE && path.starts_with("/api/memories/") {
        return Some(OpCategory::Delete);
    }
    if method != Method::POST {
        return None;
    }
    match path {
        "/v1/search" | "/v1/ambient" | "/v1/context-pack" => Some(OpCategory::Search),
        "/v1/store" | "/v1/associate" | "/v1/feedback" => Some(OpCategory::Store),
        "/v1/delete" | "/v1/purge-namespace" | "/v1/dream" | "/v1/dream/trigger" => {
            Some(OpCategory::Delete)
        }
        p if p.starts_with("/v1/purge/")
            || (p.starts_with("/api/namespace/") && p.ends_with("/purge")) =>
        {
            Some(OpCategory::Delete)
        }
        _ => None,
    }
}

/// Map an MCP tool name to a rate-limit category. `None` = unlimited
/// (`memory_status`, `memory_namespaces` — cheap local introspection).
pub fn categorize_mcp_tool(tool: &str) -> Option<OpCategory> {
    match tool {
        "memory_search" | "memory_context" | "memory_graph" | "memory_profile"
        | "memory_history" | "memory_reflect" => Some(OpCategory::Search),
        "memory_store" | "memory_ingest" | "memory_associate" | "memory_feedback" => {
            Some(OpCategory::Store)
        }
        "memory_delete" | "memory_dream" => Some(OpCategory::Delete),
        _ => None,
    }
}

/// Axum middleware enforcing per-stream limits on the mapped `/v1`/`/api`
/// paths (see [`categorize_http`]).
///
/// Must run INSIDE the auth layer: auth inserts [`AuthContext`] into request
/// extensions first, and the caller's default stream keys the bucket. A
/// missing context (passthrough/local mode) falls back to the default stream.
/// Over-limit requests get `429 Too Many Requests` with a `Retry-After`
/// header.
pub async fn rate_limit_middleware(
    limiter: Arc<RateLimiter>,
    request: Request,
    next: Next,
) -> Response {
    let Some(op) = categorize_http(request.method(), request.uri().path()) else {
        return next.run(request).await;
    };
    let stream = request
        .extensions()
        .get::<AuthContext>()
        .map_or_else(|| DEFAULT_STREAM_ID.to_string(), |a| a.stream_id.clone());
    match limiter.check(&stream, op).await {
        Ok(()) => next.run(request).await,
        Err(retry_secs) => {
            warn!(
                target: "audit",
                %stream,
                retry_secs,
                "rate limit exceeded on {} {}",
                request.method(),
                request.uri().path()
            );
            (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, retry_secs.to_string())],
                "rate limit exceeded",
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::routing::post;
    use axum::Router;
    use tower::ServiceExt;

    fn enabled_config(rpm: u32) -> RateLimitConfig {
        RateLimitConfig {
            enabled: true,
            search_rpm: rpm,
            store_rpm: rpm,
            delete_rpm: rpm,
        }
    }

    #[tokio::test]
    async fn disabled_limiter_always_allows() {
        let rl = RateLimiter::new(RateLimitConfig {
            enabled: false,
            search_rpm: 1,
            store_rpm: 1,
            delete_rpm: 1,
        });
        for _ in 0..10 {
            assert!(rl.check("s", OpCategory::Search).await.is_ok());
        }
    }

    #[tokio::test]
    async fn bucket_depletes_and_reports_retry_after() {
        let rl = RateLimiter::new(enabled_config(1));
        assert!(rl.check("s", OpCategory::Search).await.is_ok());
        let retry = rl
            .check("s", OpCategory::Search)
            .await
            .expect_err("second call within a minute must be limited");
        assert!(retry >= 1, "retry-after must be at least 1s, got {retry}");
    }

    #[tokio::test]
    async fn streams_have_independent_buckets() {
        let rl = RateLimiter::new(enabled_config(1));
        assert!(rl.check("s1", OpCategory::Search).await.is_ok());
        assert!(rl.check("s1", OpCategory::Search).await.is_err());
        assert!(
            rl.check("s2", OpCategory::Search).await.is_ok(),
            "another stream must have its own bucket"
        );
    }

    #[tokio::test]
    async fn categories_have_independent_buckets() {
        let rl = RateLimiter::new(enabled_config(1));
        assert!(rl.check("s", OpCategory::Search).await.is_ok());
        assert!(rl.check("s", OpCategory::Search).await.is_err());
        assert!(
            rl.check("s", OpCategory::Store).await.is_ok(),
            "store bucket must be independent from search"
        );
    }

    #[test]
    fn http_categorization_maps_hot_paths() {
        let post = Method::POST;
        assert_eq!(
            categorize_http(&post, "/v1/search"),
            Some(OpCategory::Search)
        );
        assert_eq!(
            categorize_http(&post, "/v1/ambient"),
            Some(OpCategory::Search)
        );
        assert_eq!(categorize_http(&post, "/v1/store"), Some(OpCategory::Store));
        assert_eq!(
            categorize_http(&post, "/v1/feedback"),
            Some(OpCategory::Store)
        );
        assert_eq!(
            categorize_http(&post, "/v1/delete"),
            Some(OpCategory::Delete)
        );
        assert_eq!(
            categorize_http(&post, "/v1/purge/abc123"),
            Some(OpCategory::Delete)
        );
        assert_eq!(
            categorize_http(&post, "/api/namespace/work/purge"),
            Some(OpCategory::Delete)
        );
        assert_eq!(
            categorize_http(&post, "/v1/dream"),
            Some(OpCategory::Delete)
        );
        assert_eq!(
            categorize_http(&Method::DELETE, "/api/memories/abc"),
            Some(OpCategory::Delete)
        );
        // Deliberately unlimited surfaces.
        assert_eq!(categorize_http(&Method::GET, "/v1/status"), None);
        assert_eq!(categorize_http(&Method::GET, "/health"), None);
        assert_eq!(categorize_http(&post, "/v1/re-embed-all"), None);
        assert_eq!(categorize_http(&post, "/v1/rebuild-tantivy"), None);
        assert_eq!(categorize_http(&Method::PUT, "/api/memories/abc"), None);
    }

    #[test]
    fn mcp_categorization_maps_tools() {
        assert_eq!(
            categorize_mcp_tool("memory_search"),
            Some(OpCategory::Search)
        );
        assert_eq!(
            categorize_mcp_tool("memory_context"),
            Some(OpCategory::Search)
        );
        assert_eq!(
            categorize_mcp_tool("memory_reflect"),
            Some(OpCategory::Search)
        );
        assert_eq!(categorize_mcp_tool("memory_store"), Some(OpCategory::Store));
        assert_eq!(
            categorize_mcp_tool("memory_ingest"),
            Some(OpCategory::Store)
        );
        assert_eq!(
            categorize_mcp_tool("memory_delete"),
            Some(OpCategory::Delete)
        );
        assert_eq!(
            categorize_mcp_tool("memory_dream"),
            Some(OpCategory::Delete)
        );
        // Cheap introspection stays unlimited.
        assert_eq!(categorize_mcp_tool("memory_status"), None);
        assert_eq!(categorize_mcp_tool("memory_namespaces"), None);
        assert_eq!(categorize_mcp_tool("not_a_tool"), None);
    }

    fn test_app(limiter: Arc<RateLimiter>) -> Router {
        Router::new()
            .route("/v1/search", post(|| async { "ok" }))
            .route("/v1/status", post(|| async { "ok" }))
            .route_layer(axum::middleware::from_fn(
                move |req: Request, next: Next| rate_limit_middleware(limiter.clone(), req, next),
            ))
    }

    fn post_req(path: &str) -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .method(Method::POST)
            .uri(path)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn middleware_returns_429_with_retry_after() {
        let app = test_app(Arc::new(RateLimiter::new(enabled_config(1))));
        let first = app.clone().oneshot(post_req("/v1/search")).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let second = app.clone().oneshot(post_req("/v1/search")).await.unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry_after = second
            .headers()
            .get(header::RETRY_AFTER)
            .expect("429 must carry Retry-After");
        assert!(retry_after.to_str().unwrap().parse::<u64>().unwrap() >= 1);
    }

    #[tokio::test]
    async fn middleware_ignores_unmapped_paths() {
        let app = test_app(Arc::new(RateLimiter::new(enabled_config(1))));
        for _ in 0..5 {
            let resp = app.clone().oneshot(post_req("/v1/status")).await.unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "unmapped path must never be limited"
            );
        }
    }
}
