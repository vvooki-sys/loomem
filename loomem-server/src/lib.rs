//! Library surface for the feature-gated Hosted engine binary.
//! The original server and its unit-test fixture remain in `main.rs`.
//!
//! The library builds no test target: the shared handler tests depend on the
//! router fixture in `main.rs` (`crate::tests`), so `main.rs` includes the
//! Hosted module under `cfg(test)` and runs its unit tests there.
#![cfg(all(feature = "hosted-engine", not(test)))]

pub mod access_hook;
pub mod auth;
pub mod content_type;
pub mod deferred;
pub mod entity_worker;
pub mod evaluation_clock;
pub mod evidence;
pub mod handlers;
pub mod local_embedding;
pub mod maintenance;
pub mod manifest;
pub mod mcp;
pub mod oauth;
pub mod rate_limiter;
pub mod resource_guards;
pub mod spa;

#[cfg(feature = "hosted-engine")]
pub mod hosted;

use loomem_core::embedding_queue::EmbeddingQueue;
use loomem_core::entity_extraction_queue::EntityExtractionQueue;
use loomem_core::graph::GraphStore;
use loomem_core::intent_log::IntentLog;
use loomem_core::local_embeddings::LocalEmbedder;
use loomem_core::query_cache::QueryCache;
use loomem_core::query_expansion::QueryExpander;
use loomem_core::workers_registry::WorkerRegistry;
use loomem_core::{
    Config, EntityExtractor, HybridSearchEngine, PiiFilter, RocksDbStore, TantivyIndex,
};
use std::sync::Arc;
use std::time::Instant;

/// Complete single-user engine state. Hosted constructs one independent value
/// per accepted generation while retaining the original single-user server.
pub struct AppState {
    pub config: Config,
    pub store: Arc<RocksDbStore>,
    pub tantivy: Arc<tokio::sync::Mutex<TantivyIndex>>,
    pub hybrid_search: HybridSearchEngine,
    pub http_client: reqwest::Client,
    pub start_time: Instant,
    pub query_expander: Arc<QueryExpander>,
    pub entity_extractor: Arc<EntityExtractor>,
    pub intent_log: Option<Arc<tokio::sync::Mutex<IntentLog>>>,
    pub embedding_queue: Option<EmbeddingQueue>,
    pub query_cache: Arc<tokio::sync::Mutex<QueryCache>>,
    pub entity_extraction_queue: Option<EntityExtractionQueue>,
    pub local_embedder: Option<Arc<LocalEmbedder>>,
    pub graph: Arc<GraphStore>,
    pub pii_filter: Arc<PiiFilter>,
    pub last_activity: Arc<tokio::sync::Mutex<Instant>>,
    #[cfg(feature = "onnx-rerank")]
    pub onnx_reranker: Option<Arc<loomem_core::onnx_reranker::OnnxReranker>>,
    pub event_tx: Option<loomem_core::event_log::EventSender>,
    pub mcp_sessions: mcp::SessionStore,
    pub workers: WorkerRegistry,
    pub deferred: Arc<deferred::DeferredTasks>,
    /// Whether status may expose counters shared by the whole process.
    pub process_diagnostics: bool,
    pub ambient_cache: Arc<loomem_core::ambient::AmbientCache>,
    pub dream_auto: Arc<loomem_core::dream_auto::DreamAutoTrigger>,
    pub rate_limiter: Arc<rate_limiter::RateLimiter>,
    /// Retrieval path and reranker counters; diagnostics only.
    pub retrieval_counters: Arc<handlers::search::RetrievalCounters>,
}
