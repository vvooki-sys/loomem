mod access_hook;
mod auth;
mod content_type;
mod handlers;
mod manifest;
mod mcp;
mod oauth;
pub mod rate_limiter;
mod resource_guards;

use anyhow::{Context, Result};
use axum::{
    middleware,
    routing::{delete, get, post},
    Router,
};
use futures::stream::StreamExt;
use loomem_core::embedding_queue::{self, EmbeddingQueue};
use loomem_core::entity_extraction_queue::{self, EntityExtractionQueue};
use loomem_core::graph::GraphStore;
use loomem_core::intent_log::IntentLog;
use loomem_core::local_embeddings::LocalEmbedder;
use loomem_core::query_cache::QueryCache;
use loomem_core::query_expansion::QueryExpander;
use loomem_core::workers_registry::{build_registry, WorkerRegistry};
use loomem_core::{
    Config, CostTracker, EntityExtractor, HybridSearchEngine, PiiFilter, RocksDbStore, TantivyIndex,
};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook_tokio::Signals;
use std::sync::Arc;
use std::time::Instant;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

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
    pub onnx_reranker: Option<loomem_core::onnx_reranker::OnnxReranker>,
    pub event_tx: Option<loomem_core::event_log::EventSender>,
    pub mcp_sessions: mcp::SessionStore,
    pub workers: WorkerRegistry,
    /// Cycle/103a: in-memory LRU cache for `/v1/ambient` Layer 1 responses.
    /// 60s TTL, 10k entry cap (configurable via `LOOMEM_AMBIENT_CACHE_TTL_SECS`
    /// — see `loomem_core::ambient::cache_ttl`). Distributed cache deferred
    /// to /103a-full per `cycles/cycle-103a-layer1-endpoint-brief.md` AC-9.
    pub ambient_cache: Arc<loomem_core::ambient::AmbientCache>,
}

/// Env var gating encryption fail-fast (cycle /144). When `true`, a missing
/// master key is a startup error instead of a silent plaintext fallback.
const EXPECT_ENABLED_ENV: &str = "LOOMEM_AT_REST_EXPECT_ENABLED";

/// Parse `LOOMEM_AT_REST_EXPECT_ENABLED` (default `false`). Truthiness:
/// `true|1` → on, `false|0` → off, anything else → `warn!` and keep the
/// default (avoids silent regression on a typo'd value).
fn at_rest_expect_enabled() -> bool {
    match std::env::var(EXPECT_ENABLED_ENV) {
        Ok(v) => match v.as_str() {
            "true" | "1" => true,
            "false" | "0" => false,
            other => {
                warn!(
                    "{EXPECT_ENABLED_ENV}={other:?} not recognized (expected true/false/1/0), defaulting to false"
                );
                false
            }
        },
        Err(_) => false,
    }
}

/// Choose the encryption provider, applying the cycle /144 fail-fast gate.
/// Pure (no env read) so the gate is unit-tested without env-var races: the
/// caller resolves `LOOMEM_AT_REST_MASTER_KEY` via `from_env` and passes the
/// result here with the `expect_enabled` flag.
///
/// - `Some(provider)` → enabled; logs the non-secret key fingerprint.
/// - `None` + `expect_enabled` → hard error (refuse to start).
/// - `None` → disabled (`NoopProvider`), unchanged MVP behaviour.
fn select_encryption_provider(
    provider: Option<loomem_core::MasterKeyEnvProvider>,
    expect_enabled: bool,
) -> Result<Arc<dyn loomem_core::EncryptionProvider>> {
    match provider {
        Some(p) => {
            info!(
                "Encryption at-rest: enabled (key fingerprint {})",
                p.fingerprint()
            );
            Ok(Arc::new(p))
        }
        None if expect_enabled => Err(anyhow::anyhow!(
            "{EXPECT_ENABLED_ENV}=true but {} is not set — refusing to start with encryption disabled",
            loomem_core::MASTER_KEY_ENV
        )),
        None => {
            info!("Encryption at-rest: disabled");
            Ok(Arc::new(loomem_core::NoopProvider))
        }
    }
}

/// Cycle /010 S5: offline re-embedding. Recomputes every stored vector with
/// the configured embedding provider/model and records the resulting
/// dimension. Intended to run with the server stopped (`loomem-server
/// --reembed`); it starts no HTTP listener and no workers.
async fn run_reembed(config: &Config, store: &RocksDbStore) -> Result<()> {
    enum Backend {
        Local(Arc<LocalEmbedder>),
        Openai {
            client: reqwest::Client,
            api_key: String,
            model: String,
        },
    }

    let provider = config.llm.embedding_provider.as_str();
    let dim = config.llm.embedding_dim;
    info!("Re-embedding with provider={provider}, target dim={dim}");

    let backend = match provider {
        "local" => {
            let model_dir = config.llm.embedding_model_path.clone().unwrap_or_else(|| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                format!("{home}/.loomem/models/multilingual-e5-small")
            });
            let embedder = loomem_core::local_embeddings::try_load(&model_dir, dim)
                .context("failed to load local embedding model for re-embedding")?;
            Backend::Local(Arc::new(embedder))
        }
        "openai" => {
            let api_key = config
                .llm
                .get_api_key()
                .context("OPENAI_API_KEY required to re-embed with the openai provider")?;
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(config.llm.timeout_secs))
                .build()
                .context("build HTTP client")?;
            Backend::Openai {
                client,
                api_key,
                model: config.llm.embedding_model.clone(),
            }
        }
        other => anyhow::bail!("unknown embedding_provider {other:?}"),
    };

    // Source of truth: every id that currently has a vector.
    let existing = store.get_all_embeddings().context("list embeddings")?;
    let total = existing.len();
    info!("Re-embedding {total} vectors");

    let mut done = 0usize;
    let mut skipped = 0usize;
    for (id, _old) in existing {
        let Some(chunk) = store
            .get_chunk(&id)
            .with_context(|| format!("read chunk {id}"))?
        else {
            skipped += 1;
            continue;
        };
        let vector = match &backend {
            Backend::Local(e) => e.embed(&chunk.content)?,
            Backend::Openai {
                client,
                api_key,
                model,
            } => loomem_core::embeddings::embed(client, api_key, model, &chunk.content).await?,
        };
        store
            .store_embedding(&id, vector)
            .with_context(|| format!("store embedding {id}"))?;
        done += 1;
        if done.is_multiple_of(200) {
            info!("  re-embedded {done}/{total}");
        }
    }

    store
        .set_embedding_dim(dim)
        .context("record embedding_dim")?;
    info!(
        "Re-embedding complete: {done} re-embedded, {skipped} skipped (no chunk), dim recorded = {dim}"
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    init_logging();

    info!("🧠 Loomem starting...");

    // Load configuration
    let config_path = std::env::var("LOOMEM_CONFIG").unwrap_or_else(|_| "config.toml".to_string());
    let config = Config::load(&config_path)
        .with_context(|| format!("Failed to load config from {}", config_path))?;

    config.log_summary();

    // Run resource guards
    resource_guards::check_resources(&config).await?;

    // Initialize storage
    let data_dir = &config.storage.data_dir;
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("Failed to create data directory: {:?}", data_dir))?;

    let rocksdb_path = data_dir.join("rocksdb");
    let store = RocksDbStore::open(&rocksdb_path, &config.storage.rocksdb)
        .context("Failed to open RocksDB")?;

    // Cycle /134 §B: build the encryption-at-rest provider and inject it into
    // the store. `MasterKeyEnvProvider` when LOOMEM_AT_REST_MASTER_KEY is set,
    // else `NoopProvider`.
    //
    // /159 boot order: attached IMMEDIATELY after RocksDB open, BEFORE any
    // chunk consumer (schema-bump Tantivy rebuild, rebuild flag, drift scan,
    // intent-log recovery). A scan that runs before this attach reads
    // encrypted rows through the default `NoopProvider` and misclassifies
    // every one of them as undecodable.
    //
    // Critical file rationale (cycle /144): adds a fail-fast leg only — when
    // LOOMEM_AT_REST_EXPECT_ENABLED=true and no master key is present, refuse
    // to start instead of silently falling back to plaintext (ADR-013 §8).
    // No change to the read/write/format paths; the enabled/disabled branches
    // are unchanged. Gate logic extracted to `select_encryption_provider`
    // (CC ≤ 10, unit-tested env-free) so `main` stays an orchestrator.
    let store = {
        let raw = loomem_core::MasterKeyEnvProvider::from_env(store.db_arc())?;
        let provider = select_encryption_provider(raw, at_rest_expect_enabled())?;
        store.with_encryption_provider(provider)
    };

    // /157 S3 (AC-5): one-line encryption state after storage open —
    // provider, fingerprint, dek_count; EncryptionStatus carries no secrets.
    info!("{}", store.encryption_provider().status().startup_line());

    // Cycle /010 S5: offline re-embedding maintenance mode. Recomputes all
    // vectors with the configured provider and records the new dimension,
    // then exits — no listener, no workers. Run with the server stopped.
    if std::env::args().any(|a| a == "--reembed") {
        return run_reembed(&config, &store).await;
    }

    // Check schema version and rebuild if needed
    const CURRENT_SCHEMA_VERSION: u32 = 13; // v13: added event_date field to Tantivy
    let stored_version = store
        .get_schema_version()
        .context("Failed to get schema version")?;

    let tantivy_path = data_dir.join("tantivy");
    let needs_rebuild = stored_version < CURRENT_SCHEMA_VERSION;

    if needs_rebuild {
        info!(
            "Schema version mismatch: stored={}, current={}",
            stored_version, CURRENT_SCHEMA_VERSION
        );
        info!("Rebuilding Tantivy index...");

        // Delete old index directory before opening with new schema
        if tantivy_path.exists() {
            std::fs::remove_dir_all(&tantivy_path)
                .context("Failed to remove old Tantivy directory")?;
        }
        std::fs::create_dir_all(&tantivy_path).context("Failed to create Tantivy directory")?;
    }

    // Initialize Tantivy (fresh dir if rebuild needed)
    let mut tantivy = TantivyIndex::open(&tantivy_path, &config.storage.tantivy)
        .context("Failed to open Tantivy index")?;

    if needs_rebuild {
        // Rebuild from RocksDB
        tantivy
            .rebuild_from_rocksdb(&store)
            .context("Failed to rebuild Tantivy index")?;

        // Update schema version
        store
            .set_schema_version(CURRENT_SCHEMA_VERSION)
            .context("Failed to update schema version")?;

        info!("Schema rebuild complete");
    } else {
        info!("Schema version OK: {}", stored_version);
    }

    // Cycle /49: explicit Tantivy rebuild flag — set by loomem-migrate after
    // chunk.stream restamp migrations. Independent of schema_version (which
    // only catches schema bumps) and drift detection (which only catches
    // count mismatches; stream-tag staleness preserves count).
    if loomem_core::storage::rebuild_tantivy_if_flag_set(&store, &mut tantivy)
        .context("Failed to check/execute tantivy_rebuild_needed flag")?
    {
        // Flag was set and rebuild completed; flag cleared inside the helper.
    }

    // Cycle /39: Tantivy drift detection — catch H3 silent-skip pattern where
    // data/tantivy/ was wiped or reset independently of meta:schema_version,
    // leaving the schema-version check unable to fire.
    let tantivy_count = tantivy.count().unwrap_or(0);
    let chunk_count = store.get_all_chunks().map(|c| c.len() as u64).unwrap_or(0);
    let drift_pct = if chunk_count > 0 {
        let abs_diff = chunk_count.abs_diff(tantivy_count);
        (abs_diff as f64 / chunk_count as f64) * 100.0
    } else {
        0.0
    };
    let drift_threshold = config.storage.tantivy.drift_warn_pct;
    if drift_pct > drift_threshold {
        warn!(
            "Tantivy drift detected: tantivy_docs={tantivy_count}, rocksdb_chunks={chunk_count}, drift={drift_pct:.2}% (threshold={drift_threshold:.2}%). Run POST /v1/rebuild-tantivy to fix."
        );
        if config.storage.tantivy.auto_rebuild_on_drift {
            info!("auto_rebuild_on_drift=true → triggering rebuild_from_rocksdb at startup");
            tantivy
                .rebuild_from_rocksdb(&store)
                .context("Failed to auto-rebuild Tantivy on drift")?;
            info!("Auto-rebuild complete");
        }
    } else {
        info!(
            "Tantivy sync OK: {tantivy_count} docs ({drift_pct:.2}% drift, threshold={drift_threshold:.2}%)"
        );
    }

    // Initialize hybrid search engine
    let hybrid_search = HybridSearchEngine::new(config.clone());

    // Create HTTP client for embeddings
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(config.llm.timeout_secs))
        .build()
        .context("Failed to create HTTP client")?;

    info!("Storage initialized successfully");

    // Initialize intent log and run recovery
    let intent_log = if config.storage.intent_log.enabled {
        let mut ilog = IntentLog::open(data_dir, &config.storage.intent_log)
            .context("Failed to open intent log")?;

        let report = loomem_core::intent_log::recover(&mut ilog, &store, &mut tantivy)
            .context("Intent log recovery failed")?;
        if report.replayed > 0 || report.skipped > 0 {
            info!(
                "Intent log recovery: {} replayed, {} skipped",
                report.replayed, report.skipped
            );
        }

        Some(Arc::new(tokio::sync::Mutex::new(ilog)))
    } else {
        info!("Intent log disabled");
        None
    };

    // Wrap store and tantivy in Arc for sharing with scheduler
    let store_arc = Arc::new(store);
    let tantivy_arc = Arc::new(tokio::sync::Mutex::new(tantivy));

    // Cycle /010 S4: refuse to start on an embedding-dimension mismatch
    // (e.g. switching provider/model without re-embedding) rather than
    // silently mixing vector sizes in hybrid search.
    store_arc
        .validate_and_record_embedding_dim(config.llm.embedding_dim)
        .context("embedding dimension check failed")?;

    // Load local embedding model if the embedding provider is "local".
    // Model dir comes from `embedding_model_path` (falls back to
    // `embedding_model`, treated as a path) when local.
    let local_embedder: Option<Arc<LocalEmbedder>> = if config.llm.embedding_provider == "local" {
        let model_dir = config.llm.embedding_model_path.clone().unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            format!("{home}/.loomem/models/multilingual-e5-small")
        });
        loomem_core::local_embeddings::try_load(&model_dir, config.llm.embedding_dim).map(Arc::new)
    } else {
        None
    };

    // Spawn embedding queue worker (if vector search enabled)
    let has_embedder = local_embedder.is_some() || config.llm.get_api_key().is_some();
    let embedding_queue = if config.storage.vector_enabled && has_embedder {
        let queue = embedding_queue::spawn_worker(
            store_arc.clone(),
            http_client.clone(),
            config.llm.clone(),
            embedding_queue::EmbeddingQueueConfig::default(),
            local_embedder.clone(),
        );
        info!(
            "Embedding queue started (embedding_provider={})",
            config.llm.embedding_provider
        );
        Some(queue)
    } else {
        info!(
            "Embedding queue disabled (vector_enabled={}, api_key={})",
            config.storage.vector_enabled,
            config.llm.get_api_key().is_some()
        );
        None
    };

    // Initialize PII filter (shared between scheduler and ingest)
    let pii_filter =
        Arc::new(PiiFilter::new(config.pii.clone()).context("Failed to initialize PII filter")?);

    // Initialize cost tracker
    let cost_tracker =
        CostTracker::new(store_arc.clone(), config.cost.clone(), http_client.clone());

    // Initialize query expander
    let query_expander =
        match QueryExpander::load(std::path::Path::new(&config.search.synonyms_file)) {
            Ok(expander) => {
                info!(
                    "Query expander loaded successfully from {}",
                    config.search.synonyms_file
                );
                Arc::new(expander)
            }
            Err(e) => {
                info!(
                    "Could not load synonyms file ({}), using empty expander: {}",
                    config.search.synonyms_file, e
                );
                Arc::new(QueryExpander::empty())
            }
        };

    // Initialize entity extractor
    let entity_extractor =
        match EntityExtractor::load(std::path::Path::new(&config.search.entities_file)) {
            Ok(extractor) => {
                info!(
                    "Entity extractor loaded successfully from {}",
                    config.search.entities_file
                );
                Arc::new(extractor)
            }
            Err(e) => {
                anyhow::bail!(
                    "Failed to load entities file ({}): {}",
                    config.search.entities_file,
                    e
                );
            }
        };

    // Initialize graph store (before scheduler, which needs it)
    let graph_store = Arc::new(GraphStore::new(store_arc.clone()));

    // Spawn entity extraction queue (LLM-based NER)
    let entity_extraction_queue =
        if config.entity_extraction.enabled && config.llm.get_api_key().is_some() {
            let cost_tracker_for_extraction =
                CostTracker::new(store_arc.clone(), config.cost.clone(), http_client.clone());
            let queue = entity_extraction_queue::spawn_worker(
                store_arc.clone(),
                graph_store.clone(),
                tantivy_arc.clone(),
                http_client.clone(),
                config.llm.clone(),
                Arc::new(cost_tracker_for_extraction),
                config.entity_extraction.clone(),
            );
            info!("Entity extraction queue started (LLM NER enabled)");
            Some(queue)
        } else {
            if config.entity_extraction.enabled {
                info!("Entity extraction disabled (no API key)");
            }
            None
        };

    // Initialize event log (before scheduler, so scheduler can emit events)
    // /150a Gap 5: surface the *effective* value at boot — the code default is
    // `false` (event_log.rs) but config.toml sets `true`, so the active value
    // depends on which config the instance loads.
    info!("event_log enabled={} (effective)", config.event_log.enabled);
    let event_tx = if config.event_log.enabled {
        match loomem_core::event_log::spawn_writer(&config.storage.data_dir, &config.event_log) {
            Ok(tx) => {
                info!("Event log started");
                Some(tx)
            }
            Err(e) => {
                tracing::warn!("Failed to start event log: {}", e);
                None
            }
        }
    } else {
        info!("Event log disabled");
        None
    };

    // Start scheduler if enabled
    let scheduler_handle = if config.scheduler.enabled {
        info!("Scheduler is enabled, starting background tasks");

        let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel(1);

        // Build the shared worker registry (used by both scheduler and handlers).
        let workers_registry = build_registry(
            &config.worker,
            config.retention.hard_purge_interval_secs,
            3600,
        );

        let scheduler = loomem_core::scheduler::Scheduler::new(
            store_arc.clone(),
            tantivy_arc.clone(),
            http_client.clone(),
            pii_filter.clone(),
            cost_tracker,
            config.clone(),
            shutdown_rx,
            intent_log.clone(),
            entity_extractor.clone(),
            graph_store.clone(),
            entity_extraction_queue.clone(),
            event_tx.clone(),
            workers_registry.clone(),
        );

        let handle = tokio::spawn(async move {
            scheduler.run().await;
        });

        Some((shutdown_tx, handle, workers_registry))
    } else {
        info!("Scheduler is disabled in config");
        None
    };

    // Load ONNX reranker if configured
    #[cfg(feature = "onnx-rerank")]
    let onnx_reranker = if config.search.rerank_enabled {
        loomem_core::onnx_reranker::try_load(config.search.rerank_model_dir.as_deref())
    } else {
        None
    };

    // Create application state
    let workers_registry = if let Some((_, _, reg)) = &scheduler_handle {
        reg.clone()
    } else {
        // Scheduler disabled: build a registry so handlers still work.
        build_registry(
            &config.worker,
            config.retention.hard_purge_interval_secs,
            3600,
        )
    };

    let state = Arc::new(AppState {
        config: config.clone(),
        graph: graph_store.clone(),
        store: store_arc,
        tantivy: tantivy_arc,
        hybrid_search,
        http_client,
        start_time: Instant::now(),
        query_expander,
        entity_extractor,
        intent_log: intent_log.clone(),
        embedding_queue,
        entity_extraction_queue,
        local_embedder: local_embedder.clone(),
        pii_filter,
        query_cache: Arc::new(tokio::sync::Mutex::new(QueryCache::new(
            config.search.cache.clone(),
        ))),
        last_activity: Arc::new(tokio::sync::Mutex::new(Instant::now())),
        #[cfg(feature = "onnx-rerank")]
        onnx_reranker,
        event_tx,
        mcp_sessions: mcp::session::new_session_store(),
        workers: workers_registry,
        ambient_cache: Arc::new(loomem_core::ambient::AmbientCache::new()),
    });

    // Resolve auth token from env var (if configured)
    let admin_token = if !config.server.auth_token_env.is_empty() {
        match std::env::var(&config.server.auth_token_env) {
            Ok(token) if !token.is_empty() => {
                info!("Auth enabled via ${}", config.server.auth_token_env);
                Some(token)
            }
            _ => {
                info!("Auth disabled (${} not set)", config.server.auth_token_env);
                None
            }
        }
    } else {
        info!("Auth disabled (no auth_token_env configured)");
        None
    };
    let auth_config = auth::AuthConfig {
        admin_token: admin_token.clone(),
    };

    // OAuth layer for MCP Remote Connector support
    let server_origin = std::env::var("SERVER_ORIGIN").unwrap_or_else(|_| {
        let p = std::env::var("PORT")
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(config.server.port);
        format!("http://{}:{}", config.server.host, p)
    });
    let oauth_state = Arc::new(oauth::OAuthState::new(server_origin));
    oauth_state.spawn_cleanup();

    let app = build_routes(RouteParams {
        auth_config,
        oauth_state,
    })
    .layer(TraceLayer::new_for_http())
    .with_state(state);

    // Setup graceful shutdown
    let signals = Signals::new([SIGTERM, SIGINT])?;
    let signals_handle = signals.handle();

    let signals_task = tokio::spawn(handle_signals(signals));

    // Start server — respect PORT env var (Railway, Render, etc.)
    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(config.server.port);
    let addr = format!("{}:{}", config.server.host, port);
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("Failed to bind to {}", addr))?;

    info!("🚀 Loomem listening on http://{}", addr);
    info!("   Endpoints:");
    info!("     POST /v1/store                    - Store events");
    info!("     POST /v1/search                   - Search memories (BM25)");
    info!("     GET  /v1/status                   - Status and config summary");
    info!("     POST /v1/retag-all                - Retag all chunks with entities");
    info!("     GET  /v1/namespaces               - List namespace→stream mappings");
    info!("     GET  /v1/generate-memory-md       - Generate MEMORY.md proposal");
    info!("     DELETE /api/memories/:id          - Delete memory by ID (brief-compliant)");
    info!("     PUT    /api/memories/:id          - Update memory content/confidence/category");
    info!("     POST /api/namespace/:ns/purge     - Purge namespace (brief-compliant)");
    info!("     GET  /health                      - Health check");

    // Rewrite POST / → POST /mcp before axum's router matches. Claude MCP clients
    // post to / after OAuth redirect, and we can't register POST / as a route
    // (axum's method matcher would then return 405 for GET /). Router::layer
    // applies middleware per-endpoint AFTER routing, so URI rewrites there have
    // no effect — wrapping the Router in an outer tower Service is the only way
    // to modify the URI before routing.
    use tower::ServiceExt;
    let rewriting = app.map_request(|mut req: axum::extract::Request| {
        if req.method() == axum::http::Method::POST && req.uri().path() == "/" {
            let new_uri = req.uri().to_string().replacen('/', "/mcp", 1);
            if let Ok(parsed) = new_uri.parse() {
                *req.uri_mut() = parsed;
            }
        }
        req
    });

    // Inject `ConnectInfo<SocketAddr>` per connection so handlers can extract
    // the remote IP.
    // `tower::make::Shared` does not propagate ConnectInfo; the standard
    // `Router::into_make_service_with_connect_info` is unavailable because
    // we need to keep the outer URI-rewrite wrap above. Custom make-service
    // clones `rewriting` per connection and wraps with a `map_request` that
    // inserts the ConnectInfo extension.
    let make_svc = MakeWithConnectInfo { inner: rewriting };

    axum::serve(listener, make_svc)
        .with_graceful_shutdown(async move {
            signals_task.await.ok();
        })
        .await
        .context("Server error")?;

    // Cleanup
    if let Some((shutdown_tx, handle, _)) = scheduler_handle {
        info!("Shutting down scheduler...");
        if let Err(e) = shutdown_tx.send(()) {
            tracing::warn!("Failed to send shutdown signal: {}", e);
        }

        // Wait for scheduler to finish (max 10s)
        match tokio::time::timeout(std::time::Duration::from_secs(10), handle).await {
            Ok(Ok(())) => info!("Scheduler shutdown complete"),
            Ok(Err(e)) => tracing::warn!("Scheduler task error: {}", e),
            Err(_) => tracing::warn!("Scheduler shutdown timeout"),
        }
    }
    // Flush intent log
    if let Some(ref ilog) = intent_log {
        let mut log = ilog.lock().await;
        if let Err(e) = log.flush() {
            tracing::warn!("Failed to flush intent log: {}", e);
        } else {
            info!("Intent log flushed");
        }
    }

    signals_handle.close();
    info!("Loomem shutdown complete");

    Ok(())
}

/// Initialize tracing. Sink is **stdout** (12-factor); centralization (shipping
/// to a SIEM/sink, retention, alerting) is an ops responsibility — see
/// `docs/SECURITY.md` § "Log centralization — ops handoff" (/150f).
///
/// **Production should set `LOOMEM_LOG_FORMAT=json`** so a downstream shipper can
/// parse structured fields. Security-relevant events are emitted with
/// `target: "audit"` (auth failures, deletes, admin actions, audit/access-audit
/// write failures) — a log shipper filters on that target to route the security
/// stream. Default (no env) is the compact human format for local dev.
fn init_logging() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let json_format = std::env::var("LOOMEM_LOG_FORMAT")
        .map(|v| v == "json")
        .unwrap_or(false);

    if json_format {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt::layer().json())
            .init();
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt::layer().compact())
            .init();
    }
}

async fn handle_signals(mut signals: Signals) {
    while let Some(signal) = signals.next().await {
        match signal {
            SIGTERM | SIGINT => {
                info!(
                    "Received shutdown signal ({}), starting graceful shutdown...",
                    signal
                );
                break;
            }
            _ => {}
        }
    }
}

/// Per-connection make-service that injects `ConnectInfo<SocketAddr>` into
/// request extensions so handlers can read the remote IP. The standard
/// `Router::into_make_service_with_connect_info` helper is unavailable here
/// because we wrap the `Router` in an outer URI-rewrite layer
/// (POST / → /mcp) above the make-service boundary.
#[derive(Clone)]
pub(crate) struct MakeWithConnectInfo<S> {
    pub(crate) inner: S,
}

impl<S> tower::Service<axum::serve::IncomingStream<'_, tokio::net::TcpListener>>
    for MakeWithConnectInfo<S>
where
    S: Clone,
{
    type Response = ConnectInfoService<S>;
    type Error = std::convert::Infallible;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(
        &mut self,
        stream: axum::serve::IncomingStream<'_, tokio::net::TcpListener>,
    ) -> Self::Future {
        let addr = *stream.remote_addr();
        std::future::ready(Ok(ConnectInfoService {
            inner: self.inner.clone(),
            addr,
        }))
    }
}

/// Per-connection wrapper that inserts `ConnectInfo<SocketAddr>` extension
/// into every request before delegating to the inner service.
#[derive(Clone)]
pub(crate) struct ConnectInfoService<S> {
    inner: S,
    addr: std::net::SocketAddr,
}

impl<S, B> tower::Service<axum::http::Request<B>> for ConnectInfoService<S>
where
    S: tower::Service<axum::http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: axum::http::Request<B>) -> Self::Future {
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo::<std::net::SocketAddr>(
                self.addr,
            ));
        self.inner.call(req)
    }
}

/// Dependencies for `build_routes()`. Kept in a struct so the function stays
/// within the ≤6 argument limit while the auth and OAuth states are
/// available to the route-layer closures.
pub(crate) struct RouteParams {
    pub(crate) auth_config: auth::AuthConfig,
    pub(crate) oauth_state: Arc<oauth::OAuthState>,
}

/// Assemble the production axum `Router` from all three route groups.
///
/// Returns the router **before** `TraceLayer` and `.with_state(state)` are
/// applied — those are added by `main()`. Extracting this function allows the
/// `router_builds_without_panic` test to exercise every `.route()` registration
/// path, catching axum-0.8 path-validation panics (the cycle/66 bug class)
/// before any deploy reaches production.
pub(crate) fn build_routes(p: RouteParams) -> Router<Arc<AppState>> {
    Router::new()
        .merge(protected_routes(&p))
        .route("/health", get(handlers::health_handler))
        .merge(oauth_routes_internal(&p))
}

/// All auth-protected routes plus their middleware layers.
///
/// §1 carve-out: declarative table, CC=1. Body is 88 `.route(path, handler)`
/// entries of uniform shape with zero branching. NLOC mechanically exceeds
/// the §1 ≤100 limit but the cognitive-overload concern §1 protects against
/// is satisfied (CC=1 — single execution path). Sub-splitting the table
/// into thematic sub-builders would relocate, not reduce, the count.
/// Source: CLAUDE.md §1 carve-out paragraph (cycle/79).
fn protected_routes(p: &RouteParams) -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/store", post(handlers::store_handler))
        .route("/v1/search", post(handlers::search_handler))
        .route("/v1/ambient", post(handlers::ambient_handler))
        .route("/v1/associate", post(handlers::associate_handler))
        .route("/v1/status", get(handlers::status_handler))
        .route("/v1/whoami", get(handlers::whoami_handler))
        .route("/v1/retag-all", post(handlers::retag_all_handler))
        .route("/v1/embed-missing", post(handlers::embed_missing_handler))
        .route("/v1/boost", post(handlers::boost_handler))
        .route("/v1/score-all", post(handlers::score_all_handler))
        .route("/v1/namespaces", get(handlers::namespaces_handler))
        .route(
            "/v1/generate-memory-md",
            get(handlers::generate_memory_md_handler),
        )
        .route("/v1/delete", post(handlers::delete_handler))
        .route(
            "/v1/purge/{id}",
            post(handlers::purge::api_purge_memory_handler),
        )
        .route(
            "/v1/purge-namespace",
            post(handlers::purge_namespace_handler),
        )
        .route("/v1/build-graph", post(handlers::build_graph_handler))
        .route(
            "/v1/extract-entities",
            post(handlers::extract_entities_handler),
        )
        .route("/v1/re-embed-all", post(handlers::re_embed_all_handler))
        .route(
            "/v1/rebuild-tantivy",
            post(handlers::rebuild_tantivy_handler),
        )
        .route(
            "/v1/health/index-sync",
            get(handlers::index_sync_health_handler),
        )
        .route("/v1/reset-backfill", post(handlers::reset_backfill_handler))
        .route(
            "/v1/backfill-event-dates",
            post(handlers::backfill_event_dates_handler),
        )
        .route(
            "/v1/admin/rekey-name-index",
            post(handlers::rekey_name_index_handler),
        )
        .route(
            "/v1/reset-importance",
            post(handlers::reset_importance_handler),
        )
        .route("/v1/tag-tier", post(handlers::tag_tier_handler))
        .route(
            "/v1/graph/entity/{name}",
            get(handlers::graph_entity_handler),
        )
        .route("/v1/graph/stats", get(handlers::graph_stats_handler))
        .route(
            "/v1/reprocess-legacy",
            post(handlers::reprocess_legacy_handler),
        )
        .route("/v1/dream", post(handlers::dream_handler))
        .route("/v1/context-pack", post(handlers::context_pack_handler))
        .route(
            "/api/memories/{id}",
            delete(handlers::api_delete_memory_handler)
                .put(handlers::api_update_memory_handler)
                .get(handlers::admin::api_get_memory_handler),
        )
        .route(
            "/api/namespace/{ns}/purge",
            post(handlers::api_purge_namespace_handler),
        )
        .route("/v1/stats", get(handlers::stats_summary_handler))
        .route("/v1/stats/summary", get(handlers::stats_summary_handler))
        .route("/v1/stats/stream/{id}", get(handlers::stats_stream_handler))
        .route(
            "/v1/stats/stream/{id}/profile",
            get(handlers::stats_profile_handler),
        )
        .route("/v1/stats/trends", get(handlers::stats_trends_handler))
        .route("/v1/stats/feedback", post(handlers::stats_feedback_handler))
        .route("/v1/feedback", post(handlers::feedback_handler))
        .route("/v1/advisory", get(handlers::advisory_handler))
        .route(
            "/v1/advisory/outcome",
            post(handlers::advisory_outcome_handler),
        )
        .route(
            "/v1/advisory/effectiveness",
            get(handlers::advisory_effectiveness_handler),
        )
        .route(
            "/v1/advisory/adjust-weights",
            post(handlers::advisory_adjust_weights_handler),
        )
        .route(
            "/v1/associations/consumed",
            post(handlers::assoc_consumed_handler),
        )
        .route(
            "/v1/dream/discoveries",
            get(handlers::dream_discoveries_handler),
        )
        .route("/v1/dream/trigger", post(handlers::dream_trigger_handler))
        // Cycle/128 — Reality bench admin endpoints.
        // Critical file rationale: additive route registration only — two new
        // routes pointing at handlers in `bench.rs`. No change to existing
        // routing or middleware. Covered by router_builds_without_panic.
        // Cycle/142 — content-type backfill (ADR-017). Additive route only;
        // admin-gated in-handler (auth.is_admin → 403). Det-only sidecar write,
        // never store_chunk. Covered by router_builds_without_panic.
        .route(
            "/v1/admin/backfill/content-type",
            post(handlers::backfill_content_type_handler),
        )
        // Cycle/147 — encrypt-at-rest backfill (ADR-013 §7). Additive route only;
        // admin-gated in-handler (non-admin → 403). Spawns background task;
        // zero changes to read/write paths. Covered by router_builds_without_panic.
        .route(
            "/v1/admin/backfill/encrypt-at-rest",
            post(handlers::encrypt_at_rest_backfill_handler),
        )
        .route(
            "/v1/admin/backfill/encrypt-at-rest/status",
            get(handlers::encrypt_at_rest_backfill_status_handler),
        )
        // Cycle/147a — graph:entity stream repair (ADR-013 §7 unblock). Additive
        // route only; admin-gated in-handler (non-admin → 403). Synchronous;
        // defaults dry_run=true (lesson /151). Covered by router_builds_without_panic.
        .route(
            "/v1/admin/repair/graph-entity-streams",
            post(handlers::graph_entity_stream_repair_handler),
        )
        .route(
            "/v1/admin/bench/run",
            post(handlers::admin_bench_run_handler),
        )
        .route(
            "/v1/admin/bench/history",
            get(handlers::admin_bench_history_handler),
        )
        // Cycle /144 — encryption-state observability (ADR-013 §8). Additive
        // route only; admin-gated in-handler (non-admin → 403). Read-only,
        // returns no key/DEK material. Covered by router_builds_without_panic.
        .route(
            "/v1/encryption/status",
            get(handlers::encryption_status_handler),
        )
        .route(
            "/mcp",
            post(mcp::handler::mcp_post_handler).delete(mcp::handler::mcp_delete_handler),
        )
        .route(
            "/admin/workers/pause",
            post(handlers::admin_workers_pause_handler),
        )
        .route(
            "/admin/workers/resume",
            post(handlers::admin_workers_resume_handler),
        )
        .route(
            "/admin/workers/status",
            get(handlers::admin_workers_status_handler),
        )
        .route(
            "/admin/workers/{name}/pause",
            post(handlers::admin_workers_pause_one_handler),
        )
        .route(
            "/admin/workers/{name}/resume",
            post(handlers::admin_workers_resume_one_handler),
        )
        .route(
            "/admin/streams/stats",
            get(handlers::admin_streams_stats_handler),
        )
        .route("/admin/ui", get(handlers::admin_ui_handler))
        // Cycle/107: PAM-inspired temporal co-occurrence POC endpoint.
        // Read-only, stream-scoped, ZERO integration with retrieval pipeline.
        .route("/v1/co_occur", get(handlers::co_occur_handler))
        .route_layer({
            let ac = p.auth_config.clone();
            middleware::from_fn(
                move |mut req: axum::extract::Request, next: middleware::Next| {
                    req.extensions_mut().insert(ac.clone());
                    auth::auth_middleware(req, next)
                },
            )
        })
}

/// OAuth 2.0 well-known + token endpoints (unauthenticated, own state).
fn oauth_routes_internal(p: &RouteParams) -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(oauth::protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(oauth::authorization_server_metadata),
        )
        .route("/oauth/register", post(oauth::register))
        .route(
            "/oauth/authorize",
            get(oauth::authorize_page).post(oauth::authorize_submit),
        )
        .route("/oauth/token", post(oauth::token))
        .with_state(p.oauth_state.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Method, Request, StatusCode};
    use loomem_core::storage::RocksDbConfig;
    use loomem_core::tantivy_index::TantivyConfig;
    use std::sync::atomic::Ordering::SeqCst;
    use tower::ServiceExt;

    /// Minimal `RouteParams` for the router-build test.
    ///
    /// `OAuthState::new` is already cheap (no I/O).
    fn minimal_route_params() -> RouteParams {
        // Auth disabled — no admin_token. Passthrough mode so
        // router_builds_without_panic does not need a credential.
        let auth_config = auth::AuthConfig { admin_token: None };
        let oauth_state = Arc::new(oauth::OAuthState::new("http://localhost".to_string()));
        RouteParams {
            auth_config,
            oauth_state,
        }
    }

    /// Build a fully-wired axum app + state for HTTP-level integration tests.
    ///
    /// Uses tempdir-backed RocksDB + in-RAM Tantivy. Scheduler disabled,
    /// embedding queue None, entity extraction None, intent log None.
    /// Auth: `Bearer admin-token` is accepted as admin (hardcoded fixture,
    /// no env var dependency — tests are deterministic in CI). (cycle/74)
    pub(crate) fn make_test_app() -> (axum::Router, Arc<AppState>) {
        make_test_app_cfg(test_config())
    }

    /// Same as [`make_test_app`] but with a caller-supplied `Config`, so tests
    /// that need a non-default toggle (e.g. `profile.enabled = true`) can opt in
    /// without duplicating the wiring.
    pub(crate) fn make_test_app_cfg(config: loomem_core::Config) -> (axum::Router, Arc<AppState>) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_cfg = RocksDbConfig {
            max_open_files: 100,
            compression: "none".to_string(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        };
        let store = Arc::new(
            loomem_core::RocksDbStore::open(tmp.path(), &db_cfg)
                .expect("RocksDbStore::open in tempdir"),
        );

        // Tantivy needs a directory on disk; use a subdir of the same tempdir.
        let tantivy_path = tmp.path().join("tantivy");
        let tantivy_cfg = TantivyConfig {
            enabled: false,
            heap_size_mb: 16,
            drift_warn_pct: 5.0,
            auto_rebuild_on_drift: false,
        };
        let tantivy = loomem_core::TantivyIndex::open(&tantivy_path, &tantivy_cfg)
            .expect("TantivyIndex::open in tempdir");

        // Keep tempdir alive for the duration of the test (store + tantivy
        // hold open file handles).
        let _kept = tmp.keep();

        let hybrid_search = loomem_core::HybridSearchEngine::new(config.clone());
        let http_client = reqwest::Client::new();
        let query_expander = Arc::new(loomem_core::query_expansion::QueryExpander::empty());
        let entity_extractor =
            Arc::new(loomem_core::EntityExtractor::load_from_str("").expect("empty extractor"));
        let graph = Arc::new(loomem_core::graph::GraphStore::new(store.clone()));
        let pii_filter = Arc::new(
            loomem_core::PiiFilter::new(loomem_core::config::PiiConfig::default())
                .expect("PiiFilter::new"),
        );
        let workers = loomem_core::workers_registry::build_registry(
            &loomem_core::scheduler::WorkerConfig::default(),
            86400,
            3600,
        );

        let state = Arc::new(AppState {
            config,
            store: store.clone(),
            tantivy: Arc::new(tokio::sync::Mutex::new(tantivy)),
            hybrid_search,
            http_client,
            start_time: std::time::Instant::now(),
            query_expander,
            entity_extractor,
            intent_log: None,
            embedding_queue: None,
            query_cache: Arc::new(tokio::sync::Mutex::new(
                loomem_core::query_cache::QueryCache::new(
                    loomem_core::query_cache::QueryCacheConfig::default(),
                ),
            )),
            entity_extraction_queue: None,
            local_embedder: None,
            graph,
            pii_filter,
            last_activity: Arc::new(tokio::sync::Mutex::new(std::time::Instant::now())),
            #[cfg(feature = "onnx-rerank")]
            onnx_reranker: None,
            event_tx: None,
            mcp_sessions: mcp::session::new_session_store(),
            workers,
            ambient_cache: Arc::new(loomem_core::ambient::AmbientCache::with_config(
                std::time::Duration::from_secs(60),
                64,
            )),
        });

        // Hardcoded fixture token: "Bearer admin-token" → admin access.
        // No env var dependency — deterministic in CI.
        let auth_config = auth::AuthConfig {
            admin_token: Some("admin-token".to_string()),
        };
        let oauth_state = Arc::new(oauth::OAuthState::new("http://localhost".to_string()));
        let params = RouteParams {
            auth_config,
            oauth_state,
        };

        let app = build_routes(params)
            .layer(TraceLayer::new_for_http())
            .with_state(state.clone());

        (app, state)
    }

    /// Build a minimal `Config` for tests. Mirrors the test_config() pattern
    /// from loomem-core's hybrid_search tests.
    fn test_config() -> loomem_core::Config {
        use loomem_core::config as cfg;
        loomem_core::Config {
            storage: cfg::StorageConfig {
                data_dir: "./data".into(),
                rocksdb: cfg::RocksDbConfig {
                    max_open_files: 100,
                    compression: "none".to_string(),
                    write_buffer_size: 4 * 1024 * 1024,
                    max_write_buffer_number: 2,
                },
                tantivy: TantivyConfig {
                    enabled: false,
                    heap_size_mb: 16,
                    drift_warn_pct: 5.0,
                    auto_rebuild_on_drift: false,
                },
                vector_enabled: false,
                intent_log: cfg::IntentLogConfig::default(),
            },
            search: cfg::SearchConfig {
                top_k: 10,
                surprise_boost: 1.5,
                hybrid_weights: cfg::HybridWeightsConfig {
                    vector: 0.6,
                    bm25: 0.4,
                },
                decay: cfg::DecayConfig {
                    l0_lambda: 0.10,
                    l1_lambda: 0.03,
                },
                synonyms_file: "synonyms.toml".to_string(),
                entities_file: "entities.toml".to_string(),
                stem_polish: false,
                rerank_enabled: false,
                rerank_candidates: 10,
                rerank_model_dir: None,
                multi_query_enabled: false,
                vector_multi_query: false,
                counting_l0_preference: false,
                importance: cfg::ImportanceConfig::default(),
                cache: loomem_core::query_cache::QueryCacheConfig::default(),
                graph: cfg::GraphSearchConfig::default(),
                complexity: cfg::ComplexityConfig::default(),
                implicit_access_boost_weight: 0.0,
            },
            advisor: cfg::AdvisorConfig::default(),
            worker: cfg::WorkerConfig::default(),
            scheduler: cfg::SchedulerConfig { enabled: false },
            llm: cfg::LlmConfig::default(),
            server: cfg::ServerConfig {
                host: "127.0.0.1".into(),
                port: 3030,
                auth_token_env: String::new(),
            },
            resource_guards: cfg::ResourceGuardsConfig::default(),
            streams: cfg::StreamsConfig::default(),
            namespaces: std::collections::HashMap::new(),
            pii: cfg::PiiConfig::default(),
            cost: cfg::CostConfig::default(),
            memory_generator: cfg::MemoryGeneratorConfig::default(),
            entity_extraction: cfg::EntityExtractionConfig::default(),
            contradiction: cfg::ContradictionConfig::default(),
            knowledge_extraction: cfg::KnowledgeExtractionConfig::default(),
            profile: cfg::ProfileConfig::default(),
            manifest: cfg::ManifestConfig::default(),
            dream: cfg::DreamConfig::default(),
            retention: cfg::RetentionConfig::default(),
            event_log: cfg::EventLogConfig::default(),
            associator: cfg::AssociatorConfig::default(),
            feedback: cfg::FeedbackConfig::default(),
            content_type: cfg::ContentTypeConfig::default(),
            access_audit: cfg::AccessAuditConfig::default(),
        }
    }

    /// Send a request through the app and collect (status, body).
    pub(crate) async fn send(
        app: axum::Router,
        req: Request<Body>,
    ) -> (StatusCode, axum::body::Bytes) {
        let resp = app.oneshot(req).await.expect("oneshot");
        let status = resp.status();
        let body = to_bytes(resp.into_body(), 64 * 1024).await.expect("body");
        (status, body)
    }

    /// Catches the cycle/66 class of bug: any `.route(":name")` panic,
    /// duplicate route, or other axum-0.8 path-validation rejection
    /// surfaces here BEFORE production deploy, instead of at server startup.
    #[test]
    fn router_builds_without_panic() {
        let _ = build_routes(minimal_route_params());
    }

    // ── Handler integration tests (cycle/74 sub-scope A) ─────────────────────

    #[tokio::test]
    async fn admin_workers_pause_one_returns_404_on_unknown_name() {
        let (app, _state) = make_test_app();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/admin/workers/nonexistent/pause")
            .header(header::AUTHORIZATION, "Bearer admin-token")
            .body(Body::empty())
            .unwrap();
        let (status, body) = send(app, req).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(
            String::from_utf8_lossy(&body).contains("unknown"),
            "body did not contain 'unknown': {}",
            String::from_utf8_lossy(&body)
        );
    }

    #[tokio::test]
    async fn admin_workers_pause_one_flips_state_on_valid_name() {
        let (app, state) = make_test_app();
        // Pre: not paused
        assert!(!state
            .workers
            .get("consolidation")
            .unwrap()
            .paused
            .load(SeqCst));

        let req = Request::builder()
            .method(Method::POST)
            .uri("/admin/workers/consolidation/pause")
            .header(header::AUTHORIZATION, "Bearer admin-token")
            .body(Body::empty())
            .unwrap();
        let (status, _body) = send(app, req).await;
        assert_eq!(status, StatusCode::OK);

        // Post: paused
        assert!(state
            .workers
            .get("consolidation")
            .unwrap()
            .paused
            .load(SeqCst));
    }

    #[tokio::test]
    async fn admin_workers_status_returns_workerinfo_array() {
        let (app, _state) = make_test_app();
        let req = Request::builder()
            .method(Method::GET)
            .uri("/admin/workers/status")
            .header(header::AUTHORIZATION, "Bearer admin-token")
            .body(Body::empty())
            .unwrap();
        let (status, body) = send(app, req).await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value =
            serde_json::from_slice(&body).expect("response is valid JSON");
        let workers = json["workers"].as_array().expect("workers is an array");
        // Use the canonical KNOWN_WORKERS list as the source of truth
        // (cycle/74-critic LOW#4) — magic `7` would silently drift if the
        // worker registry ever grows.
        assert_eq!(
            workers.len(),
            loomem_core::workers_registry::KNOWN_WORKERS.len()
        );
        assert!(
            workers.iter().any(|w| w["name"] == "consolidation"),
            "consolidation worker missing from status response"
        );
    }

    // ── Bench HTTP handler tests (cycle/128) ─────────────────────────

    /// AC2 #1: admin request to POST /v1/admin/bench/run returns 200 with task_id + started_at.
    /// Subprocess is spawned via tokio::spawn (fire-and-forget) so the HTTP response
    /// is immediate — the test does not wait for or depend on subprocess completion.
    #[tokio::test]
    async fn admin_bench_run_returns_task_id_for_admin() {
        let (app, _state) = make_test_app();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/v1/admin/bench/run")
            .header(header::AUTHORIZATION, "Bearer admin-token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let (status, body) = send(app, req).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).expect("response is valid JSON");
        assert!(v.get("task_id").is_some(), "task_id missing: {v}");
        assert!(v.get("started_at").is_some(), "started_at missing: {v}");
        // task_id is a UUID v4 string — 36 chars with 4 hyphens.
        let task_id = v["task_id"].as_str().expect("task_id is a string");
        assert_eq!(task_id.len(), 36, "task_id should be UUID-shaped");
    }

    /// AC-3 (/144): GET /v1/encryption/status as admin → 200; the test app has
    /// no master key so it reports the disabled/Noop shape and leaks no key
    /// material. Verifies endpoint wiring + admin path + AC-5 (no secrets).
    /// Non-admin 403 is covered by `handlers::bench::tests::
    /// require_admin_forbidden_rejects_non_admin` (same shared gate).
    #[tokio::test]
    async fn encryption_status_admin_returns_disabled_shape() {
        let (app, _state) = make_test_app();
        let req = Request::builder()
            .method(Method::GET)
            .uri("/v1/encryption/status")
            .header(header::AUTHORIZATION, "Bearer admin-token")
            .body(Body::empty())
            .unwrap();
        let (status, body) = send(app, req).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(v["enabled"], serde_json::json!(false));
        assert_eq!(v["provider"], serde_json::json!("noop"));
        assert!(v["master_key_fingerprint"].is_null(), "no fingerprint: {v}");
        assert!(v["master_key_version"].is_null());
        assert!(v["dek_count"].is_null());
    }

    /// AC-1 (/144): the fail-fast gate refuses to start when EXPECT_ENABLED is
    /// set but no master key is present. Pure (env-free) — exercises the gate
    /// decision directly; the actual non-zero process exit is covered by the
    /// Tier B boot-smoke.
    #[test]
    fn select_provider_no_key_expected_is_error() {
        // Cannot use `expect_err`: the Ok type `Arc<dyn EncryptionProvider>`
        // is not `Debug`. Match instead.
        let msg = match select_encryption_provider(None, true) {
            Ok(_) => panic!("expect_enabled + no key must refuse to start"),
            Err(e) => e.to_string(),
        };
        assert!(
            msg.contains(EXPECT_ENABLED_ENV) && msg.contains("refusing to start"),
            "error must name the gate and refusal: {msg}"
        );
    }

    /// AC-3 (/144): without the gate, a missing key falls back to the disabled
    /// NoopProvider — unchanged MVP behaviour.
    #[test]
    fn select_provider_no_key_unexpected_is_disabled() {
        let provider =
            select_encryption_provider(None, false).expect("no-key + no-expect must succeed");
        assert!(!provider.is_enabled());
        assert_eq!(provider.status().provider, "noop");
    }

    /// AC-2 (/144): a present key yields an enabled provider exposing a
    /// fingerprint, regardless of the expect flag.
    #[test]
    fn select_provider_with_key_is_enabled_with_fingerprint() {
        let (_app, state) = make_test_app();
        let p = loomem_core::MasterKeyEnvProvider::new([7u8; 32], state.store.db_arc());
        let provider = select_encryption_provider(Some(p), true).expect("present key must succeed");
        assert!(provider.is_enabled());
        let status = provider.status();
        assert_eq!(status.provider, "master_key_env");
        assert_eq!(status.master_key_fingerprint.as_deref(), Some("1f7e91cc"));
    }

    // AC2 #2 (non-admin → 403) is covered by the unit tests
    // `handlers::bench::tests::require_admin_forbidden_rejects_non_admin` (proves the
    // gate returns AppError::Forbidden for a non-admin AuthContext) +
    // `require_admin_forbidden_admits_admin`. AppError::Forbidden → HTTP 403 via the
    // mod.rs::IntoResponse mapping. We cannot exercise it via HTTP here because the
    // make_test_app fixture only mints an admin-token (cycle/74); a non-admin
    // bearer or session is not modelled.

    /// Minimal live chunk for the backfill test (real store, no mock — §6).
    #[cfg(test)]
    fn bf_chunk(id: &str, stream: &str, content: &str) -> loomem_core::storage::Chunk {
        loomem_core::storage::Chunk {
            id: id.to_string(),
            content: content.to_string(),
            stream: stream.to_string(),
            level: 0,
            score: 1.0,
            timestamp: 1000,
            consolidated: false,
            dormant: false,
            in_progress: false,
            prompt_version: None,
            source_ids: None,
            last_decay: None,
            metadata: None,
            importance: Some(1.0),
            persistent: false,
            last_implicit_boost: None,
            access_count: 0,
            source: None,
            created_by: None,
            updated_at: None,
            valid_from: None,
            valid_until: None,
            is_latest: true,
            superseded_by: None,
            supersedes_id: None,
            root_memory_id: None,
            version: 1,
            memory_type: None,
            extraction_meta: None,
            deleted_at: None,
            trust_level: None,
            ingester_user_id: None,
            alpha: 1.0,
            beta: 1.0,
            harmful_count: 0,
            n_ratings: 0,
            last_rated_at: None,
        }
    }

    // AC-6 (/143): the backfill (Re)classifies pre-existing chunks into the
    // sidecar via the **LLM** (stubbed — zero HTTP), NEVER rewrites the chunk
    // row (byte-identical before/after), and is idempotent via the LLM cache —
    // a second run reuses cached labels (0 extra LLM calls), overwriting the
    // sidecar with identical values (Amendment v2 §5 overwrite semantics).
    #[tokio::test]
    async fn ac6_content_type_backfill_llm_stub_leaves_chunk_untouched() {
        use loomem_core::content_type::{
            get_content_type, ClassifierSource, ContentType, ContentTypeClassifier,
            ContentTypeConfig,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Counting stub — fixed label, zero HTTP. Counts real classify calls so
        /// the cache short-circuit on the second pass is observable.
        struct CountingStub {
            calls: AtomicUsize,
        }
        impl ContentTypeClassifier for CountingStub {
            async fn classify(&self, _content: &str) -> anyhow::Result<ContentType> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(ContentType::CaseStudy)
            }
        }

        let (_app, state) = make_test_app();
        let s = "__shared_test143_backfill";
        for (id, body) in [
            ("bf1", "Release v2.0 changelog: naprawiono parser"),
            ("bf2", "Każdy PR MUSI przejść CI. ZAKAZ merge bez review."),
            (
                "bf3",
                "Zrealizowaliśmy dla klienta migrację contact center.",
            ),
        ] {
            state.store.store_chunk(&bf_chunk(id, s, body)).unwrap();
        }

        // Raw chunk bytes BEFORE backfill — must be untouched (sidecar-only).
        let raw = |id: &str| {
            state
                .store
                .db()
                .get(format!("chunk:L0:{id}"))
                .unwrap()
                .unwrap()
        };
        let before = [raw("bf1"), raw("bf2"), raw("bf3")];

        let stub = CountingStub {
            calls: AtomicUsize::new(0),
        };
        let cfg = ContentTypeConfig {
            enabled: true,
            ..ContentTypeConfig::default()
        };
        let req = handlers::admin::BackfillContentTypeRequest {
            dry_run: false,
            stream: Some(s.to_string()),
        };

        let resp = handlers::admin::run_content_type_backfill(&state.store, &stub, &cfg, &req)
            .await
            .expect("backfill ok");
        assert_eq!(resp.scanned, 3);
        assert_eq!(resp.classified, 3);
        assert_eq!(
            stub.calls.load(Ordering::SeqCst),
            3,
            "one LLM call per chunk"
        );

        // Sidecar populated with the LLM-stub type + source `llm`.
        for id in ["bf1", "bf2", "bf3"] {
            let meta = get_content_type(&state.store, id).unwrap();
            assert_eq!(meta.content_type, ContentType::CaseStudy);
            assert_eq!(meta.source, ClassifierSource::Llm);
        }

        // Chunk rows byte-identical — backfill wrote only the sidecar.
        for (i, id) in ["bf1", "bf2", "bf3"].iter().enumerate() {
            assert_eq!(raw(id), before[i], "chunk {id} row must be byte-identical");
        }

        // Idempotent via cache: second run makes 0 extra LLM calls and overwrites
        // with identical values (classified=3, all already_classified).
        let resp2 = handlers::admin::run_content_type_backfill(&state.store, &stub, &cfg, &req)
            .await
            .expect("backfill ok");
        assert_eq!(
            stub.calls.load(Ordering::SeqCst),
            3,
            "second run must hit the cache, not the LLM"
        );
        assert_eq!(resp2.classified, 3, "overwrite semantics: rewrites all 3");
        assert_eq!(resp2.already_classified, 3);
    }

    // Greptile #239 P1: dry_run=true is a FREE preview — it must make ZERO LLM
    // calls and write NOTHING to the sidecar, while still counting what would be
    // classified.
    #[tokio::test]
    async fn backfill_dry_run_makes_no_llm_calls_and_no_writes() {
        use loomem_core::content_type::{
            get_content_type, ContentType, ContentTypeClassifier, ContentTypeConfig,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingStub {
            calls: AtomicUsize,
        }
        impl ContentTypeClassifier for CountingStub {
            async fn classify(&self, _content: &str) -> anyhow::Result<ContentType> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(ContentType::CaseStudy)
            }
        }

        let (_app, state) = make_test_app();
        let s = "__shared_test143_dryrun";
        for (id, body) in [("d1", "alpha narrative"), ("d2", "beta narrative")] {
            state.store.store_chunk(&bf_chunk(id, s, body)).unwrap();
        }

        let stub = CountingStub {
            calls: AtomicUsize::new(0),
        };
        let cfg = ContentTypeConfig {
            enabled: true,
            ..ContentTypeConfig::default()
        };
        let req = handlers::admin::BackfillContentTypeRequest {
            dry_run: true,
            stream: Some(s.to_string()),
        };

        let resp = handlers::admin::run_content_type_backfill(&state.store, &stub, &cfg, &req)
            .await
            .expect("dry run ok");

        assert_eq!(
            stub.calls.load(Ordering::SeqCst),
            0,
            "dry_run must make ZERO LLM calls (Greptile #239 P1)"
        );
        assert_eq!(resp.scanned, 2);
        assert_eq!(
            resp.classified, 2,
            "preview counts what would be classified"
        );
        assert!(resp.by_type.is_empty(), "no type breakdown without LLM");
        // No sidecar entry written.
        assert_eq!(get_content_type(&state.store, "d1"), None);
        assert_eq!(get_content_type(&state.store, "d2"), None);
    }

    // Greptile follow-up: a dry_run with the classifier DISABLED must mirror the
    // real run, which classifies nothing (classify_content → None for every
    // chunk). The preview therefore reports classified=0, not classified=scanned.
    #[tokio::test]
    async fn backfill_dry_run_disabled_classifier_classifies_nothing() {
        use loomem_core::content_type::{ContentType, ContentTypeClassifier, ContentTypeConfig};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingStub {
            calls: AtomicUsize,
        }
        impl ContentTypeClassifier for CountingStub {
            async fn classify(&self, _content: &str) -> anyhow::Result<ContentType> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(ContentType::CaseStudy)
            }
        }

        let (_app, state) = make_test_app();
        let s = "__shared_test143_dryrun_disabled";
        for (id, body) in [("dd1", "alpha narrative"), ("dd2", "beta narrative")] {
            state.store.store_chunk(&bf_chunk(id, s, body)).unwrap();
        }

        let stub = CountingStub {
            calls: AtomicUsize::new(0),
        };
        let cfg = ContentTypeConfig {
            enabled: false,
            ..ContentTypeConfig::default()
        };
        let req = handlers::admin::BackfillContentTypeRequest {
            dry_run: true,
            stream: Some(s.to_string()),
        };

        let resp = handlers::admin::run_content_type_backfill(&state.store, &stub, &cfg, &req)
            .await
            .expect("dry run ok");

        assert_eq!(
            stub.calls.load(Ordering::SeqCst),
            0,
            "dry_run must make ZERO LLM calls"
        );
        assert_eq!(resp.scanned, 2);
        assert_eq!(
            resp.classified, 0,
            "disabled classifier classifies nothing — preview must match the real run"
        );
        assert!(resp.by_type.is_empty(), "no type breakdown without LLM");
    }

    // ── /151 — POST /v1/backfill-event-dates ─────────────────────────

    /// /151: chunk carrying an extracted event_date whose valid_from still
    /// points at the ingest timestamp (pre-/151 state).
    fn event_date_chunk(id: &str, stream: &str, event_date: &str) -> loomem_core::storage::Chunk {
        let mut chunk = bf_chunk(id, stream, "Anna wdrożył Loomem 1992-12-01");
        chunk.valid_from = Some(1000);
        chunk.extraction_meta = Some(loomem_core::storage::ExtractionMeta {
            fact_type: loomem_core::storage::FactType::Fact,
            subject: None,
            event_date: Some(event_date.to_string()),
            event_date_context: None,
            supersedes: None,
            superseded_by: None,
            confidence: 0.9,
            extracted_from: None,
            extraction_model: None,
            original_content: None,
        });
        chunk
    }

    /// AC-3 (/151): the backfill endpoint is admin-only — a non-admin
    /// AuthContext gets AppError::Forbidden (HTTP 403 via the mod.rs
    /// IntoResponse mapping). Exercised on the handler directly because
    /// make_test_app only mints an admin token (same approach as the
    /// require_admin gate tests).
    #[tokio::test]
    async fn backfill_event_dates_forbidden_for_non_admin() {
        use crate::auth::{AuthContext, KeyScope, StreamMembership};
        use loomem_core::storage::UserRole;

        let (_app, state) = make_test_app();
        let auth = AuthContext {
            stream_id: "__user_a__".to_string(),
            user_id: Some("user_a".to_string()),
            is_admin: false,
            role: UserRole::Writer,
            scope: KeyScope::Private,
            memberships: vec![StreamMembership {
                stream_id: "__user_a__".to_string(),
                role: UserRole::Writer,
                source: KeyScope::Private,
            }],
        };
        let result = handlers::admin::backfill_event_dates_handler(
            axum::extract::State(state),
            axum::Extension(auth),
            None,
        )
        .await;
        match result {
            Err(handlers::AppError::Forbidden(_)) => {}
            other => panic!("non-admin must get Forbidden, got {:?}", other.is_ok()),
        }
    }

    /// AC-3 (/151): dry_run counts the would-be rewrite but writes nothing —
    /// chunk bytes byte-identical before/after. The loop signature carries no
    /// LLM client, so zero paid calls by construction (/143 rule). The real
    /// run then rewrites valid_from and a re-run is an idempotent skip.
    #[tokio::test]
    async fn backfill_event_dates_dry_run_writes_nothing_then_real_run_updates() {
        let (_app, state) = make_test_app();
        let s = "__shared_test151_backfill";
        state
            .store
            .store_chunk(&event_date_chunk("ed1", s, "1992-12-01"))
            .unwrap();

        let raw_before = state.store.db().get("chunk:L0:ed1").unwrap().unwrap();

        let dry = handlers::admin::run_event_date_backfill(&state.store, true).expect("dry ok");
        assert_eq!(dry.with_event_date, 1);
        assert_eq!(dry.updated, 1, "preview counts the would-be rewrite");
        let raw_after_dry = state.store.db().get("chunk:L0:ed1").unwrap().unwrap();
        assert_eq!(raw_before, raw_after_dry, "dry_run must write NOTHING");

        let real = handlers::admin::run_event_date_backfill(&state.store, false).expect("run ok");
        assert_eq!(real.updated, 1);
        assert_eq!(real.errors, 0);
        let chunk = state.store.get_chunk("ed1").unwrap().expect("chunk");
        // 1992-12-01 00:00:00 UTC
        assert_eq!(chunk.valid_from, Some(723_168_000));

        let rerun = handlers::admin::run_event_date_backfill(&state.store, false).expect("ok");
        assert_eq!(rerun.updated, 0, "idempotent: second run skips");
        assert_eq!(rerun.skipped_already_correct, 1);
    }

    /// AC-5 (/151): chunks without extraction_meta (or without event_date)
    /// pass through the backfill untouched — backward compat for pre-/151
    /// data. Unparseable dates are counted, not rewritten.
    #[tokio::test]
    async fn backfill_event_dates_skips_chunks_without_event_date() {
        let (_app, state) = make_test_app();
        let s = "__shared_test151_compat";
        state
            .store
            .store_chunk(&bf_chunk("nometa", s, "legacy chunk"))
            .unwrap();
        state
            .store
            .store_chunk(&event_date_chunk("badts", s, "not a date"))
            .unwrap();

        let resp = handlers::admin::run_event_date_backfill(&state.store, false).expect("ok");
        assert_eq!(resp.with_event_date, 1, "only the chunk carrying a date");
        assert_eq!(resp.skipped_unparseable_date, 1);
        assert_eq!(resp.updated, 0);
        assert_eq!(resp.errors, 0);
        let nometa = state.store.get_chunk("nometa").unwrap().expect("chunk");
        assert_eq!(nometa.valid_from, None, "untouched");
    }

    /// /151 route wiring (post-#126 lesson): POST /v1/backfill-event-dates
    /// with the admin token reaches the handler and returns 200.
    #[tokio::test]
    async fn backfill_event_dates_route_wired_for_admin() {
        let (app, _state) = make_test_app();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/v1/backfill-event-dates")
            .header(header::AUTHORIZATION, "Bearer admin-token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"dry_run": true}"#))
            .unwrap();
        let (status, body) = send(app, req).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(v["status"], serde_json::json!("completed"));
        assert_eq!(v["dry_run"], serde_json::json!(true));
    }

    // ── DELETE /api/memories/:id contract tests (cycle/129, ADR-012) ──────
    //
    // Pins the 200 / 207 / 404 status-code mapping defined by
    // `docs/decisions/ADR-012-delete-contract.md` and implemented in
    // `handlers/admin.rs::api_delete_memory_handler` +
    // `handlers/delete.rs::delete_memory_fully`.
    //
    // make_test_app() is pub(crate) in the binary; per the cycle/129 brief
    // (AC2 alternative — "lub równoważny inline w istniejącym integration
    // tests module") these live here rather than in tests/.

    fn delete_contract_test_chunk(id: &str) -> loomem_core::storage::Chunk {
        loomem_core::storage::Chunk {
            id: id.to_string(),
            content: format!("delete contract fixture for {id}"),
            stream: "__user_test__".to_string(),
            level: 0,
            score: 0.5,
            timestamp: 1_700_000_000,
            consolidated: false,
            dormant: false,
            in_progress: false,
            prompt_version: None,
            source_ids: None,
            last_decay: None,
            metadata: None,
            importance: None,
            persistent: false,
            last_implicit_boost: None,
            access_count: 0,
            source: None,
            created_by: None,
            updated_at: None,
            valid_from: None,
            valid_until: None,
            is_latest: true,
            superseded_by: None,
            supersedes_id: None,
            root_memory_id: None,
            version: 1,
            memory_type: None,
            extraction_meta: None,
            deleted_at: None,
            trust_level: None,
            ingester_user_id: None,
            alpha: 1.0,
            beta: 1.0,
            harmful_count: 0,
            n_ratings: 0,
            last_rated_at: None,
        }
    }

    /// ADR-012 / cycle/129 AC2 (a): happy path — all four delete steps
    /// succeed → HTTP 200 + body `{"deleted": true, "id": "<id>"}`.
    #[tokio::test]
    async fn delete_existing_chunk_returns_200_all_ok() {
        let (app, state) = make_test_app();
        let id = "delete-contract-200-ok";
        state
            .store
            .store_chunk(&delete_contract_test_chunk(id))
            .expect("store_chunk");

        let req = Request::builder()
            .method(Method::DELETE)
            .uri(format!("/api/memories/{id}"))
            .header(header::AUTHORIZATION, "Bearer admin-token")
            .body(Body::empty())
            .unwrap();
        let (status, body) = send(app, req).await;

        assert_eq!(
            status,
            StatusCode::OK,
            "happy path must return 200 (ADR-012)"
        );
        let parsed: serde_json::Value =
            serde_json::from_slice(&body).expect("200 response body must be JSON per ADR-012");
        assert_eq!(parsed["deleted"], serde_json::json!(true));
        assert_eq!(parsed["id"], serde_json::json!(id));
        assert!(
            parsed.get("partial").is_none(),
            "200 body must NOT carry the 207 partial marker: {parsed}"
        );

        let after = state
            .store
            .get_chunk(id)
            .expect("get_chunk")
            .expect("chunk present");
        assert!(
            after.deleted_at.is_some(),
            "chunk must be soft-deleted after 200"
        );
    }

    /// ADR-012 / cycle/129 AC2 (b): non-existent ID → store_deleted=false
    /// → HTTP 404 + body `{"error": "not found"}`. Replaces the pre-/117
    /// "HTTP 500 by design" Kwiatek A claim from the memory-hygiene skill.
    #[tokio::test]
    async fn delete_nonexistent_returns_404() {
        let (app, _state) = make_test_app();
        let req = Request::builder()
            .method(Method::DELETE)
            .uri("/api/memories/00000000-0000-0000-0000-000000000000")
            .header(header::AUTHORIZATION, "Bearer admin-token")
            .body(Body::empty())
            .unwrap();
        let (status, body) = send(app, req).await;

        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "non-existent ID must return 404 (ADR-012), not 500"
        );
        let parsed: serde_json::Value =
            serde_json::from_slice(&body).expect("404 response body must be JSON per ADR-012");
        assert_eq!(parsed["error"], serde_json::json!("not found"));
    }

    /// ADR-012 / cycle/129 AC2 (c) — MANDATORY (operator decision Q1
    /// LOCKED). Partial-success: store + tantivy + embedding succeed but
    /// graph step fails → HTTP 207 with per-step breakdown.
    ///
    /// Approach: corrupt the `graph:chunk:<id>` reverse-index by writing
    /// malformed bytes that fail JSON deserialize in
    /// `get_entities_for_chunk`. This mirrors the unit-test pattern in
    /// `handlers/delete.rs::test_delete_partial_success_graph_fails` —
    /// the only difference is that we drive it through the full HTTP
    /// handler instead of calling `delete_memory_fully` directly, so the
    /// 207 status-code mapping in `api_delete_memory_handler` is exercised.
    ///
    /// No mock GraphStore needed — the corruption approach hits the same
    /// failure path that fires in production when entity rows drift in
    /// schema (Kwiatek A pre-/117).
    #[tokio::test]
    async fn delete_with_graph_failure_returns_207() {
        let (app, state) = make_test_app();
        let id = "delete-contract-207-graph-fail";
        state
            .store
            .store_chunk(&delete_contract_test_chunk(id))
            .expect("store_chunk");
        // Poison the graph chunk reverse-index. remove_chunk_references()
        // calls get_entities_for_chunk(), which deserializes this value as
        // JSON and returns Err on garbage bytes.
        state
            .store
            .put(
                format!("graph:chunk:{id}").as_bytes(),
                b"\xff\xfe not valid json",
            )
            .expect("put corrupted graph:chunk index");

        let req = Request::builder()
            .method(Method::DELETE)
            .uri(format!("/api/memories/{id}"))
            .header(header::AUTHORIZATION, "Bearer admin-token")
            .body(Body::empty())
            .unwrap();
        let (status, body) = send(app, req).await;

        assert_eq!(
            status,
            StatusCode::MULTI_STATUS,
            "graph step failure must return 207, not 500 (ADR-012)"
        );
        let parsed: serde_json::Value =
            serde_json::from_slice(&body).expect("207 response body must be JSON per ADR-012");
        assert_eq!(parsed["deleted"], serde_json::json!(true));
        assert_eq!(parsed["id"], serde_json::json!(id));
        assert_eq!(parsed["partial"], serde_json::json!(true));
        assert_eq!(
            parsed["steps"]["store"],
            serde_json::json!("ok"),
            "207 schema: steps.store always 'ok' (fatal store errors → 500)"
        );
        assert_eq!(parsed["steps"]["tantivy"], serde_json::json!("ok"));
        assert_eq!(parsed["steps"]["embedding"], serde_json::json!("ok"));
        assert_eq!(
            parsed["steps"]["graph"],
            serde_json::json!("error"),
            "207 schema: steps.graph == 'error' when graph step fails"
        );
        // errors.graph must carry the deserialize error message (non-null).
        assert!(
            parsed["errors"]["graph"].is_string(),
            "207 schema: errors.graph must be a string when steps.graph == 'error', got {}",
            parsed["errors"]["graph"]
        );

        // Critical post-condition: chunk is still soft-deleted in store
        // (cycle/117 fix — no short-circuit before store soft-delete).
        let after = state
            .store
            .get_chunk(id)
            .expect("get_chunk")
            .expect("chunk present");
        assert!(
            after.deleted_at.is_some(),
            "store soft-delete must succeed even when graph step fails (cycle/117)"
        );
    }

    // ── POST /v1/purge/:id contract tests (cycle/135, GDPR Art 17) ────────
    //
    // Admin-path HTTP coverage via make_test_app() + oneshot. Non-admin
    // RBAC paths (D3 cross-stream → 400, D4 shared → 403) live in
    // handlers::purge::tests against `enforce_purge_rbac` — make_test_app
    // mints only an admin-token fixture, so RBAC split-coverage matches
    // cycle/128 lesson `feedback_make_test_app_admin_only_split_coverage`.

    /// AC9 happy path: existing chunk → 200 + body `{purged, id, skipped_soft}`.
    #[tokio::test]
    async fn purge_existing_chunk_returns_200_skip_soft_true() {
        let (app, state) = make_test_app();
        let id = "purge-contract-200-ok";
        state
            .store
            .store_chunk(&delete_contract_test_chunk(id))
            .expect("store_chunk");

        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/v1/purge/{id}"))
            .header(header::AUTHORIZATION, "Bearer admin-token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"reason":"GDPR Art 17 contract test"}"#))
            .unwrap();
        let (status, body) = send(app, req).await;

        assert_eq!(status, StatusCode::OK, "happy path must return 200");
        let parsed: serde_json::Value = serde_json::from_slice(&body).expect("200 body JSON");
        assert_eq!(parsed["purged"], serde_json::json!(true));
        assert_eq!(parsed["id"], serde_json::json!(id));
        assert_eq!(
            parsed["skipped_soft"],
            serde_json::json!(true),
            "fixture chunk had deleted_at=None → skipped_soft must be true"
        );
        assert!(
            parsed.get("partial").is_none(),
            "200 body must not carry 207 partial marker: {parsed}"
        );

        assert!(
            state.store.get_chunk(id).expect("get_chunk").is_none(),
            "chunk must be hard-deleted from store after 200"
        );
    }

    /// AC9 missing-chunk: non-existent ID → 404 + `{purged: false, id}`.
    #[tokio::test]
    async fn purge_nonexistent_returns_404() {
        let (app, _state) = make_test_app();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/v1/purge/00000000-0000-0000-0000-000000000000")
            .header(header::AUTHORIZATION, "Bearer admin-token")
            .body(Body::empty())
            .unwrap();
        let (status, body) = send(app, req).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        let parsed: serde_json::Value = serde_json::from_slice(&body).expect("404 body JSON");
        assert_eq!(parsed["purged"], serde_json::json!(false));
        assert_eq!(
            parsed["id"],
            serde_json::json!("00000000-0000-0000-0000-000000000000")
        );
    }

    /// AC9 D8 idempotency: store → purge 200 → repeat purge 404.
    #[tokio::test]
    async fn purge_idempotent_repeat_returns_404() {
        let (app, state) = make_test_app();
        let id = "purge-contract-idempotent";
        state
            .store
            .store_chunk(&delete_contract_test_chunk(id))
            .expect("store_chunk");

        // First purge: 200.
        let req1 = Request::builder()
            .method(Method::POST)
            .uri(format!("/v1/purge/{id}"))
            .header(header::AUTHORIZATION, "Bearer admin-token")
            .body(Body::empty())
            .unwrap();
        let (status1, _) = send(app.clone(), req1).await;
        assert_eq!(status1, StatusCode::OK, "first purge → 200");

        // Second purge of same id: 404.
        let req2 = Request::builder()
            .method(Method::POST)
            .uri(format!("/v1/purge/{id}"))
            .header(header::AUTHORIZATION, "Bearer admin-token")
            .body(Body::empty())
            .unwrap();
        let (status2, body2) = send(app, req2).await;
        assert_eq!(status2, StatusCode::NOT_FOUND, "repeat purge → 404");
        let parsed: serde_json::Value = serde_json::from_slice(&body2).expect("404 body JSON");
        assert_eq!(parsed["purged"], serde_json::json!(false));
    }

    /// D6: reason field longer than `MAX_REASON_LEN` chars → 400.
    #[tokio::test]
    async fn purge_oversized_reason_returns_400() {
        let (app, state) = make_test_app();
        let id = "purge-contract-oversized-reason";
        state
            .store
            .store_chunk(&delete_contract_test_chunk(id))
            .expect("store_chunk");

        // 501 'a' chars > MAX_REASON_LEN (500).
        let body_str = format!(r#"{{"reason":"{}"}}"#, "a".repeat(501));
        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/v1/purge/{id}"))
            .header(header::AUTHORIZATION, "Bearer admin-token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body_str))
            .unwrap();
        let (status, _) = send(app, req).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "oversized reason → 400");

        // Post-condition: chunk still present (handler short-circuited before cascade).
        assert!(
            state.store.get_chunk(id).expect("get_chunk").is_some(),
            "rejected request must not delete the chunk"
        );
    }

    // ── /150e-2 access-audit hooks + read surface ─────────────────────

    #[tokio::test]
    async fn access_audit_noop_when_disabled() {
        // AC7: feature off (default) → record_access writes nothing.
        let (_app, state) = make_test_app();
        let auth = crate::auth::AuthContext::single_stream(
            "s-test",
            crate::auth::UserRole::Admin,
            crate::auth::KeyScope::Shared,
            Some("u1".into()),
            true,
        );
        crate::access_hook::record_access(
            &state,
            &auth,
            loomem_core::access_audit::AccessOp::Search,
            None,
            3,
        );
        let listing =
            loomem_core::access_audit::list_access(&state.store, "s-test", 10).expect("list");
        assert!(
            listing.records.is_empty(),
            "disabled feature must write no access records (AC7)"
        );
    }

    // ── cycle/147: encrypt-at-rest backfill HTTP tests ────────────────────────

    /// T7 (AC-6 HTTP): make_test_app uses NoopProvider; a fresh valid token must
    /// be rejected with 400 and a message about the missing master key.
    #[tokio::test]
    async fn t7_backfill_encrypt_noop_provider_returns_400() {
        let (app, _state) = make_test_app();
        // Use today's date to ensure a valid token format.
        let today = chrono::Utc::now().format("%Y%m%d").to_string();
        let token = format!("snap-{today}-prod-aabb1234");
        let body = serde_json::json!({ "snapshot_token": token });
        let req = Request::builder()
            .method(Method::POST)
            .uri("/v1/admin/backfill/encrypt-at-rest")
            .header(header::AUTHORIZATION, "Bearer admin-token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let (status, resp_body) = send(app, req).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "NoopProvider must return 400"
        );
        let v: serde_json::Value = serde_json::from_slice(&resp_body).expect("valid JSON");
        let err_msg = v["error"].as_str().expect("error field present");
        assert!(
            err_msg.contains("NoopProvider")
                || err_msg.contains("disabled")
                || err_msg.contains("LOOMEM_AT_REST_MASTER_KEY"),
            "error must explain missing master key: {err_msg}"
        );
    }

    /// T8 (AC-3 HTTP): non-admin AuthContext must get Forbidden (403).
    /// Uses synthetic AuthContext (make_test_app is admin-only per feedback rule).
    #[tokio::test]
    async fn t8_backfill_encrypt_non_admin_returns_403() {
        use crate::auth::{AuthContext, KeyScope, StreamMembership};
        use loomem_core::storage::UserRole;

        let (_app, state) = make_test_app();
        let auth = AuthContext {
            stream_id: "__user_x__".to_string(),
            user_id: Some("user_x".to_string()),
            is_admin: false,
            role: UserRole::Writer,
            scope: KeyScope::Private,
            memberships: vec![StreamMembership {
                stream_id: "__user_x__".to_string(),
                role: UserRole::Writer,
                source: KeyScope::Private,
            }],
        };
        let today = chrono::Utc::now().format("%Y%m%d").to_string();
        let req_body = handlers::backfill_encrypt::BackfillEncryptRequest {
            snapshot_token: format!("snap-{today}-prod-aabb1234"),
            batch_size: 200,
            inter_batch_sleep_ms: 0,
        };
        let result = handlers::backfill_encrypt::encrypt_at_rest_backfill_handler(
            axum::extract::State(state),
            axum::Extension(auth),
            axum::Json(req_body),
        )
        .await;
        match result {
            Err(handlers::AppError::Forbidden(_)) => {}
            Ok(_) => panic!("non-admin must get Forbidden, got Ok"),
            Err(other) => panic!("non-admin must get Forbidden, got: {other:?}"),
        }
    }

    /// T9 (GET status never_run): fresh app returns `{"status":"never_run"}` with HTTP 200.
    #[tokio::test]
    async fn t9_backfill_encrypt_status_never_run() {
        let (app, _state) = make_test_app();
        let req = Request::builder()
            .method(Method::GET)
            .uri("/v1/admin/backfill/encrypt-at-rest/status")
            .header(header::AUTHORIZATION, "Bearer admin-token")
            .body(Body::empty())
            .unwrap();
        let (status, body) = send(app, req).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(v["status"], serde_json::json!("never_run"));
    }

    // ── cycle/147a: graph-entity stream repair HTTP tests ─────────────────────

    /// T8 (AC-3 HTTP): non-admin must get Forbidden (403).
    /// Uses synthetic AuthContext (make_test_app is admin-only per feedback rule).
    #[tokio::test]
    async fn t8_graph_repair_non_admin_returns_403() {
        use crate::auth::{AuthContext, KeyScope, StreamMembership};
        use loomem_core::storage::UserRole;

        let (_app, state) = make_test_app();
        let auth = AuthContext {
            stream_id: "__user_y__".to_string(),
            user_id: Some("user_y".to_string()),
            is_admin: false,
            role: UserRole::Writer,
            scope: KeyScope::Private,
            memberships: vec![StreamMembership {
                stream_id: "__user_y__".to_string(),
                role: UserRole::Writer,
                source: KeyScope::Private,
            }],
        };
        let result = handlers::graph_repair::graph_entity_stream_repair_handler(
            axum::extract::State(state),
            axum::Extension(auth),
            None,
        )
        .await;
        match result {
            Err(handlers::AppError::Forbidden(_)) => {}
            Ok(_) => panic!("non-admin must get Forbidden, got Ok"),
            Err(other) => panic!("non-admin must get Forbidden, got: {other:?}"),
        }
    }

    /// T9 (admin gate + noop refusal, no-body path): admin with NO body passes
    /// the gate and gets the NoopProvider 400 (not 403, no panic). NOTE: this
    /// does NOT exercise the dry_run default — the noop check fires before the
    /// body is read; the serde/no-body default is unit-tested in
    /// `handlers::graph_repair::tests` (critic /147a MED-3: honest test titles).
    #[tokio::test]
    async fn t9_graph_repair_noop_refusal_with_no_body() {
        use crate::auth::{AuthContext, KeyScope, StreamMembership};
        use loomem_core::storage::UserRole;

        let (_app, state) = make_test_app();
        let auth = AuthContext {
            stream_id: "__admin__".to_string(),
            user_id: Some("admin".to_string()),
            is_admin: true,
            role: UserRole::Admin,
            scope: KeyScope::Private,
            memberships: vec![StreamMembership {
                stream_id: "__admin__".to_string(),
                role: UserRole::Admin,
                source: KeyScope::Private,
            }],
        };
        // No body → dry_run defaults to true; but NoopProvider returns 400.
        // We verify the 400 is returned (not 403), proving admin gate passed
        // and dry_run default was applied (no panic from missing body).
        let result = handlers::graph_repair::graph_entity_stream_repair_handler(
            axum::extract::State(state),
            axum::Extension(auth),
            None,
        )
        .await;
        // NoopProvider → 400 response (not Err::Forbidden, not panic).
        match result {
            Ok((status, _body)) => {
                assert_eq!(
                    status,
                    StatusCode::BAD_REQUEST,
                    "T9: NoopProvider must return 400"
                );
            }
            Err(handlers::AppError::Forbidden(_)) => {
                panic!("T9: admin must not get Forbidden")
            }
            Err(other) => panic!("T9: unexpected error: {other:?}"),
        }
    }

    // ── cycle/157 S3: status observability (AC-6) ─────────────────────────

    /// AC-6 (/157): `/v1/status` carries `undecodable_chunks` (populated
    /// after a full scan) and the per-category `llm_failures_recent` block.
    #[tokio::test]
    async fn test_http_status_reports_undecodable_and_llm_failures() {
        let (app, state) = make_test_app();
        // Run the canonical full scan so the counter is Some(0) (clean DB).
        let _ = state.store.get_all_chunks().expect("full scan");

        let req = Request::builder()
            .method(Method::GET)
            .uri("/v1/status")
            .header(header::AUTHORIZATION, "Bearer admin-token")
            .body(Body::empty())
            .expect("request");
        let (status, body) = send(app, req).await;
        assert_eq!(status, StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(&body).expect("valid JSON");
        assert_eq!(
            json["undecodable_chunks"],
            serde_json::json!(0),
            "clean test DB after a full scan reports zero: {json}"
        );
        let llm = &json["llm_failures_recent"];
        for key in [
            "extraction",
            "ner",
            "embedding",
            "consolidation",
            "window_secs",
        ] {
            assert!(
                llm.get(key).is_some(),
                "llm_failures_recent.{key} present: {json}"
            );
        }
    }
}
