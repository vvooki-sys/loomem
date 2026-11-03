//! Per-generation wiring of the original single-user engine.
use crate::{deferred::DeferredTasks, entity_worker, maintenance, rate_limiter, AppState};
use anyhow::{Context, Result};
use loomem_core::embedding_queue::{self, EmbeddingQueueConfig};
use loomem_core::event_log::{EventLogWorker, EventSender};
use loomem_core::graph::GraphStore;
use loomem_core::intent_log::IntentLog;
use loomem_core::local_embeddings::LocalEmbedder;
use loomem_core::query_cache::QueryCache;
use loomem_core::query_expansion::QueryExpander;
use loomem_core::scheduler::Scheduler;
use loomem_core::workers_registry::build_registry;
use loomem_core::{Config, CostTracker, EntityExtractor, PiiFilter, RocksDbStore, TantivyIndex};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::task::JoinHandle;

const CURRENT_SCHEMA_VERSION: u32 = 13;

/// Point-in-time depth of the scope's asynchronous queues.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub struct QueueDepths {
    pub embedding_pending: usize,
    pub entity_pending: usize,
    pub event_pending: usize,
}

pub struct ScopeRuntime {
    pub state: Arc<AppState>,
    pub scheduler: Arc<Scheduler>,
    pub root: PathBuf,
    scheduler_shutdown: tokio::sync::broadcast::Sender<()>,
    scheduler_task: Option<JoinHandle<()>>,
    event_worker: Option<EventLogWorker>,
    accepting_writes: bool,
    quiesced: bool,
}

struct ScopeServices {
    config: Config,
    store: Arc<RocksDbStore>,
    tantivy: Arc<tokio::sync::Mutex<TantivyIndex>>,
    http_client: reqwest::Client,
    intent_log: Option<Arc<tokio::sync::Mutex<IntentLog>>>,
    pii_filter: Arc<PiiFilter>,
    entity_extractor: Arc<EntityExtractor>,
    graph: Arc<GraphStore>,
    local_embedder: Option<Arc<LocalEmbedder>>,
    embedding_queue: Option<loomem_core::embedding_queue::EmbeddingQueue>,
    entity_extraction_queue: Option<loomem_core::entity_extraction_queue::EntityExtractionQueue>,
    event_tx: Option<EventSender>,
    event_worker: Option<EventLogWorker>,
}

impl ScopeServices {
    fn open(
        mut config: Config,
        root: &Path,
        embedder: Option<Arc<LocalEmbedder>>,
        rebuild: bool,
    ) -> Result<Self> {
        config.storage.data_dir = root.to_path_buf();
        std::fs::create_dir_all(root).context("create Hosted scope root")?;
        let (store, tantivy) = open_storage(&config, root, rebuild)?;
        let store = Arc::new(store);
        let tantivy = Arc::new(tokio::sync::Mutex::new(tantivy));
        store.validate_and_record_embedding_dim(config.llm.embedding_dim)?;
        let local_embedder = match embedder {
            Some(embedder) => Some(embedder),
            None => crate::local_embedding::load(&config.llm)?,
        };
        let http_client = config.llm.build_http_client()?;
        let intent_log = open_intent_log(&config, &store, tantivy.clone())?;
        let pii_filter = Arc::new(PiiFilter::new(config.pii.clone())?);
        let entity_extractor = Arc::new(EntityExtractor::load(Path::new(
            &config.search.entities_file,
        ))?);
        let graph = Arc::new(GraphStore::new(store.clone()));
        Ok(Self {
            config,
            store,
            tantivy,
            http_client,
            intent_log,
            pii_filter,
            entity_extractor,
            graph,
            local_embedder,
            embedding_queue: None,
            entity_extraction_queue: None,
            event_tx: None,
            event_worker: None,
        })
    }

    fn start_queues(&mut self, reader: bool) -> Result<()> {
        if reader {
            return Ok(());
        }
        self.embedding_queue = start_embedding_queue(
            &self.config,
            self.store.clone(),
            self.http_client.clone(),
            self.local_embedder.clone(),
        );
        self.entity_extraction_queue = entity_worker::start(
            &self.config,
            self.store.clone(),
            self.graph.clone(),
            self.tantivy.clone(),
            self.http_client.clone(),
            self.pii_filter.clone(),
        );
        (self.event_tx, self.event_worker) = start_event_log(&self.config)?;
        Ok(())
    }

    fn into_state(self) -> Result<(Arc<AppState>, Option<EventLogWorker>)> {
        let config = self.config;
        let query_expander = QueryExpander::load(Path::new(&config.search.synonyms_file))
            .map_or_else(|_| Arc::new(QueryExpander::empty()), Arc::new);
        #[cfg(feature = "onnx-rerank")]
        let onnx_reranker = config
            .search
            .rerank_enabled
            .then(|| {
                loomem_core::onnx_reranker::try_load(config.search.rerank_model_dir.as_deref())
            })
            .flatten()
            .map(Arc::new);
        let workers = build_registry(
            &config.worker,
            config.retention.hard_purge_interval_secs,
            3600,
        );
        let hybrid_search = crate::evaluation_clock::engine(config.clone())?;
        let state = Arc::new(AppState {
            rate_limiter: Arc::new(rate_limiter::RateLimiter::new(config.rate_limit.clone())),
            dream_auto: dream_trigger(&config),
            hybrid_search,
            query_expander,
            entity_extractor: self.entity_extractor,
            intent_log: self.intent_log,
            embedding_queue: self.embedding_queue,
            query_cache: Arc::new(tokio::sync::Mutex::new(QueryCache::new(
                config.search.cache.clone(),
            ))),
            entity_extraction_queue: self.entity_extraction_queue,
            local_embedder: self.local_embedder,
            graph: self.graph,
            pii_filter: self.pii_filter,
            start_time: Instant::now(),
            last_activity: Arc::new(tokio::sync::Mutex::new(Instant::now())),
            #[cfg(feature = "onnx-rerank")]
            onnx_reranker,
            event_tx: self.event_tx,
            mcp_sessions: crate::mcp::session::new_session_store(),
            workers,
            deferred: Arc::new(DeferredTasks::default()),
            process_diagnostics: false,
            retrieval_counters: Arc::new(crate::handlers::search::RetrievalCounters::default()),
            ambient_cache: Arc::new(loomem_core::ambient::AmbientCache::new()),
            store: self.store,
            tantivy: self.tantivy,
            http_client: self.http_client,
            config,
        });
        Ok((state, self.event_worker))
    }
}

impl ScopeRuntime {
    pub async fn open(
        config: Config,
        root: &Path,
        local_embedder: Option<Arc<LocalEmbedder>>,
        rebuild_index: bool,
    ) -> Result<Self> {
        Self::open_mode(config, root, local_embedder, rebuild_index, false).await
    }

    pub async fn open_reader(
        config: Config,
        root: &Path,
        local_embedder: Option<Arc<LocalEmbedder>>,
    ) -> Result<Self> {
        Self::open_mode(config, root, local_embedder, false, true).await
    }

    async fn open_mode(
        config: Config,
        root: &Path,
        local_embedder: Option<Arc<LocalEmbedder>>,
        rebuild_index: bool,
        reader: bool,
    ) -> Result<Self> {
        let mut services = ScopeServices::open(config, root, local_embedder, rebuild_index)?;
        services.start_queues(reader)?;
        let (state, event_worker) = services.into_state()?;
        let (scheduler_shutdown, shutdown_rx) = tokio::sync::broadcast::channel(1);
        let scheduler = build_scheduler(&state, shutdown_rx);
        if !reader {
            scheduler.recover_orphaned().await?;
        }
        let scheduler_task = (!reader && state.config.scheduler.enabled).then(|| {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.run().await })
        });
        Ok(Self {
            state,
            scheduler,
            root: root.to_path_buf(),
            scheduler_shutdown,
            scheduler_task,
            event_worker,
            accepting_writes: true,
            quiesced: false,
        })
    }

    pub fn is_quiesced(&self) -> bool {
        self.quiesced
    }

    pub fn accepts_writes(&self) -> bool {
        self.accepting_writes && !self.quiesced
    }

    /// True only when the ONNX cross-encoder was really loaded for this
    /// generation; `try_load` otherwise degrades silently to the HTTP reranker.
    pub fn rerank_loaded(&self) -> bool {
        #[cfg(feature = "onnx-rerank")]
        {
            self.state.onnx_reranker.is_some()
        }
        #[cfg(not(feature = "onnx-rerank"))]
        {
            false
        }
    }

    /// Accepted background work that is not yet durable, per queue.
    pub fn queue_depths(&self) -> QueueDepths {
        QueueDepths {
            embedding_pending: self
                .state
                .embedding_queue
                .as_ref()
                .map_or(0, |queue| queue.pending()),
            entity_pending: self
                .state
                .entity_extraction_queue
                .as_ref()
                .map_or(0, |queue| queue.pending()),
            event_pending: self
                .event_worker
                .as_ref()
                .map_or(0, |worker| worker.queued()),
        }
    }

    /// Stop every writer owned by this generation before Hosted accepts it.
    pub async fn quiesce(&mut self) -> Result<()> {
        if self.quiesced {
            return Ok(());
        }
        self.accepting_writes = false;
        self.stop_scheduler().await?;
        self.state
            .deferred
            .drain()
            .await
            .context("drain deferred")?;
        self.stop_queues().await?;
        self.flush().await?;
        self.quiesced = true;
        Ok(())
    }

    async fn stop_scheduler(&mut self) -> Result<()> {
        let _ = self.scheduler_shutdown.send(());
        if let Some(task) = self.scheduler_task.take() {
            task.await.context("join Hosted scheduler")?;
        }
        self.scheduler.wait_for_idle().await;
        Ok(())
    }

    async fn stop_queues(&self) -> Result<()> {
        if let Some(queue) = &self.state.embedding_queue {
            queue.shutdown().await?;
        }
        if let Some(queue) = &self.state.entity_extraction_queue {
            queue.shutdown().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        if let Some(worker) = self.event_worker.take() {
            worker.shutdown().await?;
        }
        if let Some(log) = &self.state.intent_log {
            log.lock().await.flush()?;
        }
        #[cfg(feature = "evaluation")]
        if evaluation_quiesce_failure() {
            anyhow::bail!("synthetic final Tantivy commit failure");
        }
        self.state.tantivy.lock().await.commit()?;
        self.state.store.db_arc().flush().context("flush RocksDB")?;
        Ok(())
    }
}

#[cfg(feature = "evaluation")]
fn evaluation_quiesce_failure() -> bool {
    static FIRST_FAILURE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
    std::env::var("LOOMEM_EVALUATION_QUIESCE_FAIL").as_deref() == Ok("1")
        && FIRST_FAILURE.swap(false, std::sync::atomic::Ordering::Relaxed)
}

fn build_scheduler(
    state: &Arc<AppState>,
    shutdown_rx: tokio::sync::broadcast::Receiver<()>,
) -> Arc<Scheduler> {
    Arc::new(Scheduler::new(
        state.store.clone(),
        state.tantivy.clone(),
        state.http_client.clone(),
        state.pii_filter.clone(),
        CostTracker::new(
            state.store.clone(),
            state.config.cost.clone(),
            state.http_client.clone(),
        ),
        state.config.clone(),
        shutdown_rx,
        state.intent_log.clone(),
        state.entity_extractor.clone(),
        state.graph.clone(),
        state.entity_extraction_queue.clone(),
        state.local_embedder.clone(),
        state.event_tx.clone(),
        state.workers.clone(),
    ))
}

fn dream_trigger(config: &Config) -> Arc<loomem_core::dream_auto::DreamAutoTrigger> {
    let threshold = if config.dream.enabled {
        config.dream.auto_trigger_threshold
    } else {
        0
    };
    Arc::new(loomem_core::dream_auto::DreamAutoTrigger::new(
        threshold,
        config.dream.auto_cooldown_secs,
    ))
}

fn start_embedding_queue(
    config: &Config,
    store: Arc<RocksDbStore>,
    client: reqwest::Client,
    embedder: Option<Arc<LocalEmbedder>>,
) -> Option<loomem_core::embedding_queue::EmbeddingQueue> {
    if !config.storage.vector_enabled {
        return None;
    }
    if embedder.is_none() && config.llm.get_api_key().is_none() {
        return None;
    }
    Some(embedding_queue::spawn_worker(
        store,
        client,
        config.llm.clone(),
        EmbeddingQueueConfig::default(),
        embedder,
    ))
}

fn start_event_log(config: &Config) -> Result<(Option<EventSender>, Option<EventLogWorker>)> {
    if !config.event_log.enabled {
        return Ok((None, None));
    }
    let (sender, worker) =
        loomem_core::event_log::spawn_writer_managed(&config.storage.data_dir, &config.event_log)?;
    Ok((Some(sender), Some(worker)))
}

fn open_intent_log(
    config: &Config,
    store: &Arc<RocksDbStore>,
    tantivy: Arc<tokio::sync::Mutex<TantivyIndex>>,
) -> Result<Option<Arc<tokio::sync::Mutex<IntentLog>>>> {
    if !config.storage.intent_log.enabled {
        return Ok(None);
    }
    let mut log = IntentLog::open(&config.storage.data_dir, &config.storage.intent_log)?;
    let mut index = tantivy
        .try_lock()
        .map_err(|_| anyhow::anyhow!("new Hosted Tantivy lock is busy"))?;
    loomem_core::intent_log::recover(&mut log, store, &mut index)?;
    Ok(Some(Arc::new(tokio::sync::Mutex::new(log))))
}

fn open_storage(
    config: &Config,
    root: &Path,
    rebuild_index: bool,
) -> Result<(RocksDbStore, TantivyIndex)> {
    let store = open_store(config, root)?;
    let tantivy = open_tantivy(config, root, &store, rebuild_index)?;
    Ok((store, tantivy))
}

fn open_store(config: &Config, root: &Path) -> Result<RocksDbStore> {
    let store = RocksDbStore::open(root.join("rocksdb"), &config.storage.rocksdb)?;
    let provider = loomem_core::MasterKeyEnvProvider::from_env(store.db_arc())?;
    match provider {
        Some(provider) => Ok(store.with_encryption_provider(Arc::new(provider))),
        None if std::env::var("LOOMEM_AT_REST_EXPECT_ENABLED").as_deref() == Ok("1") => {
            anyhow::bail!("Hosted scope requires at-rest encryption")
        }
        None => Ok(store.with_encryption_provider(Arc::new(loomem_core::NoopProvider))),
    }
}

fn open_tantivy(
    config: &Config,
    root: &Path,
    store: &RocksDbStore,
    rebuild_index: bool,
) -> Result<TantivyIndex> {
    let path = root.join("tantivy");
    let rebuild = rebuild_index || store.get_schema_version()? < CURRENT_SCHEMA_VERSION;
    if rebuild && path.exists() {
        std::fs::remove_dir_all(&path)?;
    }
    let mut tantivy = TantivyIndex::open(&path, &config.storage.tantivy)?;
    rebuild_tantivy(&mut tantivy, store, rebuild)?;
    loomem_core::storage::rebuild_tantivy_if_flag_set(store, &mut tantivy)?;
    maintenance::check_index_drift(&config.storage.tantivy, store, &mut tantivy)?;
    Ok(tantivy)
}

fn rebuild_tantivy(tantivy: &mut TantivyIndex, store: &RocksDbStore, rebuild: bool) -> Result<()> {
    if rebuild {
        tantivy.rebuild_from_rocksdb(store)?;
        store.set_schema_version(CURRENT_SCHEMA_VERSION)?;
    }
    Ok(())
}
