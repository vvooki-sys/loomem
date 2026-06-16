use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use crate::workers_registry::{is_worker_paused, now_epoch, WorkerRegistry};
use tokio::sync::broadcast;
use tokio::time::{interval, interval_at, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::config::Config;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerConfig {
    pub consolidation: crate::consolidation::ConsolidationConfig,
    pub decay_worker: crate::decay::DecayWorkerConfig,
    pub compaction: crate::config::CompactionConfig,
    pub clustering: crate::associator::clustering::ClusteringConfig,
    #[serde(default)]
    pub backup: crate::config::BackupConfig,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            consolidation: crate::consolidation::ConsolidationConfig {
                interval_secs: 300,
                batch_size: 50,
                concurrency: 2,
                timeout_secs: 300,
                min_chunks_to_consolidate: 3,
                min_age_secs: 600,
                prompt_version: 1,
                consolidation_style: "observation".to_string(),
                similarity_threshold: 0.3,
                quality_gate_threshold: 0.5,
            },
            decay_worker: crate::decay::DecayWorkerConfig {
                interval_secs: 3600,
                factor: 0.995,
                timeout_secs: 60,
                l0_factor: 0.990,
                l1_factor: 0.995,
                dormant_threshold: 0.01,
                access_boost: true,
                adaptive_enabled: false,
                adaptive_dampening: 0.5,
                adaptive_cap: 200,
                persistent_auto_threshold: 50,
                persistent_auto_enabled: true,
            },
            compaction: crate::config::CompactionConfig {
                interval_secs: 3600,
                timeout_secs: 60,
            },
            clustering: crate::associator::clustering::ClusteringConfig {
                interval_secs: 21600,
                max_iterations: 1000,
                timeout_secs: 60,
            },
            backup: crate::config::BackupConfig::default(),
        }
    }
}
use crate::consolidation;
use crate::cost_tracker::CostTracker;
use crate::decay;
use crate::entity_extractor::EntityExtractor;
use crate::event_log::{self, EventSender, MemoryEvent};
use crate::graph::GraphStore;
use crate::intent_log::IntentLog;
use crate::pii_filter::PiiFilter;
use crate::stats_aggregator::StatsAggregator;
use crate::storage::RocksDbStore;
use crate::tantivy_index::TantivyIndex;

pub struct Scheduler {
    storage: Arc<RocksDbStore>,
    tantivy: Arc<tokio::sync::Mutex<TantivyIndex>>,
    llm_client: reqwest::Client,
    pii_filter: Arc<PiiFilter>,
    cost_tracker: Arc<CostTracker>,
    config: Config,
    shutdown_rx: broadcast::Receiver<()>,
    intent_log: Option<Arc<tokio::sync::Mutex<IntentLog>>>,
    entity_extractor: Arc<EntityExtractor>,
    graph: Arc<GraphStore>,
    entity_extraction_queue: Option<crate::entity_extraction_queue::EntityExtractionQueue>,
    event_tx: Option<EventSender>,
    pub workers: WorkerRegistry,

    // Task guards
    consolidation_running: Arc<AtomicBool>,
    decay_running: Arc<AtomicBool>,
    compaction_running: Arc<AtomicBool>,
    backup_running: Arc<AtomicBool>,
    clustering_running: Arc<AtomicBool>,
    purge_running: Arc<AtomicBool>,
    stats_running: Arc<AtomicBool>,

    // Failure tracking
    consolidation_failures: Arc<AtomicU64>,
    decay_failures: Arc<AtomicU64>,
    compaction_failures: Arc<AtomicU64>,
}

impl Scheduler {
    pub fn new(
        storage: Arc<RocksDbStore>,
        tantivy: Arc<tokio::sync::Mutex<TantivyIndex>>,
        llm_client: reqwest::Client,
        pii_filter: Arc<PiiFilter>,
        cost_tracker: CostTracker,
        config: Config,
        shutdown_rx: broadcast::Receiver<()>,
        intent_log: Option<Arc<tokio::sync::Mutex<IntentLog>>>,
        entity_extractor: Arc<EntityExtractor>,
        graph: Arc<GraphStore>,
        entity_extraction_queue: Option<crate::entity_extraction_queue::EntityExtractionQueue>,
        event_tx: Option<EventSender>,
        workers: WorkerRegistry,
    ) -> Self {
        Self {
            storage,
            tantivy,
            llm_client,
            pii_filter,
            cost_tracker: Arc::new(cost_tracker),
            config,
            shutdown_rx,
            intent_log,
            entity_extractor,
            graph,
            entity_extraction_queue,
            event_tx,
            workers,
            consolidation_running: Arc::new(AtomicBool::new(false)),
            decay_running: Arc::new(AtomicBool::new(false)),
            compaction_running: Arc::new(AtomicBool::new(false)),
            backup_running: Arc::new(AtomicBool::new(false)),
            clustering_running: Arc::new(AtomicBool::new(false)),
            purge_running: Arc::new(AtomicBool::new(false)),
            stats_running: Arc::new(AtomicBool::new(false)),
            consolidation_failures: Arc::new(AtomicU64::new(0)),
            decay_failures: Arc::new(AtomicU64::new(0)),
            compaction_failures: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Recover orphaned chunks with in_progress=true on startup
    pub async fn recover_orphaned(&self) -> Result<()> {
        info!("Recovering orphaned in_progress chunks...");

        let count = self.storage.recover_orphaned_chunks()?;

        if count > 0 {
            info!("Recovered {} orphaned chunks", count);
        } else {
            info!("No orphaned chunks found");
        }

        Ok(())
    }

    /// Returns true if the named worker is paused.
    fn is_paused(&self, name: &str) -> bool {
        is_worker_paused(&self.workers, name)
    }

    /// Run scheduler with tokio::select!
    pub async fn run(mut self) {
        info!("🚀 Scheduler started");

        // Recover orphaned chunks first
        if let Err(e) = self.recover_orphaned().await {
            error!("Failed to recover orphaned chunks: {}", e);
        }

        let consolidation_interval = self.config.worker.consolidation.interval_secs;
        let decay_interval = self.config.worker.decay_worker.interval_secs;
        let compaction_interval = self.config.worker.compaction.interval_secs;

        let backup_interval = self.config.worker.backup.interval_secs;
        let backup_enabled = self.config.worker.backup.enabled;

        let clustering_interval = self.config.worker.clustering.interval_secs;
        let clustering_enabled = self.config.associator.enabled;

        let purge_interval = self.config.retention.hard_purge_interval_secs;

        let stats_interval: u64 = 3600; // hourly stats aggregation

        let mut consolidation_ticker = interval(Duration::from_secs(consolidation_interval));
        let mut decay_ticker = interval(Duration::from_secs(decay_interval));
        let mut compaction_ticker = interval(Duration::from_secs(compaction_interval));
        // Delay first tick so server restarts don't trigger immediate checkpoints.
        let mut backup_ticker = interval_at(
            tokio::time::Instant::now() + Duration::from_secs(backup_interval),
            Duration::from_secs(backup_interval),
        );
        let mut clustering_ticker = interval(Duration::from_secs(clustering_interval));
        let mut purge_ticker = interval(Duration::from_secs(purge_interval));
        let mut stats_ticker = interval(Duration::from_secs(stats_interval));

        // Skip first tick (immediate)
        consolidation_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        decay_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        compaction_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        backup_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        clustering_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        purge_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        stats_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Run initial clustering on startup if enabled and no clusters exist
        if clustering_enabled {
            self.spawn_clustering_task();
        }

        loop {
            tokio::select! {
                _ = consolidation_ticker.tick() => self.tick_consolidation(),
                _ = decay_ticker.tick() => self.tick_decay(),
                _ = compaction_ticker.tick() => self.tick_compaction(),
                _ = backup_ticker.tick(), if backup_enabled => self.tick_backup(),
                _ = clustering_ticker.tick(), if clustering_enabled => self.tick_clustering(),
                _ = purge_ticker.tick() => self.tick_purge(),
                _ = stats_ticker.tick() => self.tick_stats(),
                _ = self.shutdown_rx.recv() => {
                    info!("Scheduler received shutdown signal");
                    break;
                }
            }
        }

        self.wait_for_running_tasks_to_complete().await;
        info!("Scheduler shutdown complete");
    }

    fn tick_consolidation(&self) {
        if self.is_paused("consolidation") {
            return;
        }
        if self.consolidation_running.load(Ordering::SeqCst) {
            warn!("Consolidation already running, skipping tick");
            return;
        }
        self.spawn_consolidation_task();
    }

    fn tick_decay(&self) {
        if self.is_paused("decay") {
            return;
        }
        if self.decay_running.load(Ordering::SeqCst) {
            warn!("Decay already running, skipping tick");
            return;
        }
        self.spawn_decay_task();
    }

    fn tick_compaction(&self) {
        if self.is_paused("compaction") {
            return;
        }
        if self.compaction_running.load(Ordering::SeqCst) {
            warn!("Compaction already running, skipping tick");
            return;
        }
        self.spawn_compaction_task();
    }

    fn tick_backup(&self) {
        if self.is_paused("backup") {
            return;
        }
        if self.backup_running.load(Ordering::SeqCst) {
            warn!("Backup already running, skipping tick");
            return;
        }
        self.spawn_backup_task();
    }

    fn tick_clustering(&self) {
        if self.is_paused("clustering") {
            return;
        }
        if self.clustering_running.load(Ordering::SeqCst) {
            warn!("Clustering already running, skipping tick");
            return;
        }
        self.spawn_clustering_task();
    }

    fn tick_purge(&self) {
        if self.is_paused("purge") {
            return;
        }
        if self.purge_running.load(Ordering::SeqCst) {
            warn!("Purge already running, skipping tick");
            return;
        }
        self.spawn_purge_task();
    }

    fn tick_stats(&self) {
        if self.is_paused("stats") {
            return;
        }
        if self.stats_running.load(Ordering::SeqCst) {
            warn!("Stats aggregation already running, skipping tick");
            return;
        }
        self.spawn_stats_task();
    }

    async fn wait_for_running_tasks_to_complete(&self) {
        // Wait for running tasks (max 10s)
        info!("Waiting for running tasks to complete (max 10s)...");

        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            let all_stopped = !self.consolidation_running.load(Ordering::SeqCst)
                && !self.decay_running.load(Ordering::SeqCst)
                && !self.compaction_running.load(Ordering::SeqCst)
                && !self.clustering_running.load(Ordering::SeqCst)
                && !self.purge_running.load(Ordering::SeqCst)
                && !self.stats_running.load(Ordering::SeqCst);

            if all_stopped {
                info!("All tasks stopped gracefully");
                break;
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn spawn_consolidation_task(&self) {
        let storage = self.storage.clone();
        let tantivy = self.tantivy.clone();
        let llm_client = self.llm_client.clone();
        let llm_config = self.config.llm.clone();
        let pii_filter = self.pii_filter.clone();
        let cost_tracker = self.cost_tracker.clone();
        let config = self.config.worker.consolidation.clone();
        let running = self.consolidation_running.clone();
        let failures = self.consolidation_failures.clone();
        let timeout = Duration::from_secs(self.config.worker.consolidation.timeout_secs);
        let intent_log = self.intent_log.clone();
        let entity_extractor = self.entity_extractor.clone();
        let graph = self.graph.clone();
        let entity_extraction_queue = self.entity_extraction_queue.clone();
        let event_tx = self.event_tx.clone();
        let workers = self.workers.clone();

        running.store(true, Ordering::SeqCst);

        tokio::spawn(async move {
            let cancel = CancellationToken::new();
            let start = std::time::Instant::now();

            if let Some(w) = workers.get("consolidation") {
                w.last_run_at.store(now_epoch(), Ordering::SeqCst);
            }

            info!("Starting consolidation task");

            let result = tokio::time::timeout(
                timeout,
                consolidation::consolidate(
                    storage,
                    tantivy,
                    &llm_client,
                    &llm_config,
                    &pii_filter,
                    &cost_tracker,
                    &config,
                    cancel,
                    intent_log,
                    Some(entity_extractor),
                    Some(graph),
                    entity_extraction_queue,
                ),
            )
            .await;

            match result {
                Ok(Ok(report)) => {
                    info!(
                        "Consolidation completed: consolidated={}, skipped={}, errors={}, cost=${:.4}, duration={:.2}s",
                        report.consolidated_count,
                        report.skipped_count,
                        report.errors,
                        report.cost_usd,
                        start.elapsed().as_secs_f64()
                    );

                    if let Some(w) = workers.get("consolidation") {
                        // truncation intentional: usize → u64 widens on 32-bit, identity on 64-bit
                        w.items_processed_total
                            .fetch_add(report.consolidated_count as u64, Ordering::SeqCst);
                        w.last_success_at.store(now_epoch(), Ordering::SeqCst);
                    }

                    // Emit consolidation event
                    if let Some(ref tx) = event_tx {
                        event_log::emit(
                            tx,
                            MemoryEvent::Consolidation {
                                input_count: report.consolidated_count + report.skipped_count,
                                output_count: report.consolidated_count,
                                dropped_ids: vec![],
                                cost_usd: report.cost_usd,
                            },
                        );
                    }

                    failures.store(0, Ordering::SeqCst); // Reset failure counter
                }
                Ok(Err(e)) => {
                    let count = failures.fetch_add(1, Ordering::SeqCst) + 1;
                    error!(
                        "Consolidation failed (consecutive failures: {}): {}",
                        count, e
                    );

                    if count >= 3 {
                        error!("⚠️ Consolidation failed 3 times consecutively!");
                    }
                }
                Err(_) => {
                    let count = failures.fetch_add(1, Ordering::SeqCst) + 1;
                    error!(
                        "Consolidation timed out after {}s (consecutive failures: {})",
                        timeout.as_secs(),
                        count
                    );

                    if count >= 3 {
                        error!("⚠️ Consolidation timed out 3 times consecutively!");
                    }
                }
            }

            running.store(false, Ordering::SeqCst);
        });
    }

    fn spawn_decay_task(&self) {
        let storage = self.storage.clone();
        let config = self.config.worker.decay_worker.clone();
        let running = self.decay_running.clone();
        let failures = self.decay_failures.clone();
        let timeout = Duration::from_secs(self.config.worker.decay_worker.timeout_secs);
        let workers = self.workers.clone();

        running.store(true, Ordering::SeqCst);

        tokio::spawn(async move {
            let cancel = CancellationToken::new();
            let start = std::time::Instant::now();

            if let Some(w) = workers.get("decay") {
                w.last_run_at.store(now_epoch(), Ordering::SeqCst);
            }

            info!("Starting decay task");

            let result =
                tokio::time::timeout(timeout, decay::run_decay(storage, &config, cancel)).await;

            match result {
                Ok(Ok(report)) => {
                    info!(
                        "Decay completed: decayed={}, dormant={}, errors={}, duration={:.2}s",
                        report.decayed_count,
                        report.dormant_count,
                        report.errors,
                        start.elapsed().as_secs_f64()
                    );
                    // decay does not return a useful item count; +1 per run
                    if let Some(w) = workers.get("decay") {
                        w.items_processed_total.fetch_add(1, Ordering::SeqCst);
                        w.last_success_at.store(now_epoch(), Ordering::SeqCst);
                    }
                    failures.store(0, Ordering::SeqCst); // Reset failure counter
                }
                Ok(Err(e)) => {
                    let count = failures.fetch_add(1, Ordering::SeqCst) + 1;
                    error!("Decay failed (consecutive failures: {}): {}", count, e);

                    if count >= 3 {
                        error!("⚠️ Decay failed 3 times consecutively!");
                    }
                }
                Err(_) => {
                    let count = failures.fetch_add(1, Ordering::SeqCst) + 1;
                    error!(
                        "Decay timed out after {}s (consecutive failures: {})",
                        timeout.as_secs(),
                        count
                    );

                    if count >= 3 {
                        error!("⚠️ Decay timed out 3 times consecutively!");
                    }
                }
            }

            running.store(false, Ordering::SeqCst);
        });
    }

    fn spawn_compaction_task(&self) {
        let storage = self.storage.clone();
        let running = self.compaction_running.clone();
        let failures = self.compaction_failures.clone();
        let timeout = Duration::from_secs(self.config.worker.compaction.timeout_secs);
        let intent_log = self.intent_log.clone();
        let workers = self.workers.clone();

        running.store(true, Ordering::SeqCst);

        tokio::spawn(async move {
            let start = std::time::Instant::now();

            if let Some(w) = workers.get("compaction") {
                w.last_run_at.store(now_epoch(), Ordering::SeqCst);
            }

            info!("Starting compaction task");

            let result = tokio::time::timeout(timeout, async { storage.compact() }).await;

            match result {
                Ok(Ok(())) => {
                    // GC old intent log archives alongside RocksDB compaction
                    if let Some(ref ilog) = intent_log {
                        let log = ilog.lock().await;
                        if let Err(e) = log.gc_old_logs() {
                            warn!("Intent log GC failed: {}", e);
                        }
                    }
                    info!(
                        "Compaction completed: duration={:.2}s",
                        start.elapsed().as_secs_f64()
                    );
                    if let Some(w) = workers.get("compaction") {
                        w.items_processed_total.fetch_add(1, Ordering::SeqCst);
                        w.last_success_at.store(now_epoch(), Ordering::SeqCst);
                    }
                    failures.store(0, Ordering::SeqCst); // Reset failure counter
                }
                Ok(Err(e)) => {
                    let count = failures.fetch_add(1, Ordering::SeqCst) + 1;
                    error!("Compaction failed (consecutive failures: {}): {}", count, e);

                    if count >= 3 {
                        error!("⚠️ Compaction failed 3 times consecutively!");
                    }
                }
                Err(_) => {
                    let count = failures.fetch_add(1, Ordering::SeqCst) + 1;
                    error!(
                        "Compaction timed out after {}s (consecutive failures: {})",
                        timeout.as_secs(),
                        count
                    );

                    if count >= 3 {
                        error!("⚠️ Compaction timed out 3 times consecutively!");
                    }
                }
            }

            running.store(false, Ordering::SeqCst);
        });
    }

    fn spawn_clustering_task(&self) {
        let storage = self.storage.clone();
        let config = self.config.associator.clone();
        let running = self.clustering_running.clone();
        let timeout = Duration::from_secs(self.config.worker.clustering.timeout_secs);
        let graph = self.graph.clone();
        let event_tx = self.event_tx.clone();
        let workers = self.workers.clone();

        running.store(true, Ordering::SeqCst);

        tokio::spawn(async move {
            let start = std::time::Instant::now();

            if let Some(w) = workers.get("clustering") {
                w.last_run_at.store(now_epoch(), Ordering::SeqCst);
            }

            info!("Starting ECA clustering task");

            let result = tokio::time::timeout(
                timeout,
                std::future::ready(run_clustering_pass(storage, config, graph, event_tx)),
            )
            .await;

            match result {
                Ok((clusters, chunks)) => {
                    info!(
                        "ECA clustering completed: {} clusters across {} chunks, duration={:.2}s",
                        clusters,
                        chunks,
                        start.elapsed().as_secs_f64()
                    );
                    if let Some(w) = workers.get("clustering") {
                        w.items_processed_total.fetch_add(1, Ordering::SeqCst);
                        w.last_success_at.store(now_epoch(), Ordering::SeqCst);
                    }
                }
                Err(_) => {
                    error!("ECA clustering timed out after {}s", timeout.as_secs());
                }
            }

            running.store(false, Ordering::SeqCst);
        });
    }

    fn spawn_purge_task(&self) {
        let storage = self.storage.clone();
        let graph = self.graph.clone();
        let tantivy = self.tantivy.clone();
        let running = self.purge_running.clone();
        let retention_days = self.config.retention.soft_delete_days;
        let workers = self.workers.clone();

        running.store(true, Ordering::SeqCst);

        tokio::spawn(async move {
            let start = std::time::Instant::now();

            if let Some(w) = workers.get("purge") {
                w.last_run_at.store(now_epoch(), Ordering::SeqCst);
            }

            info!("Starting hard-purge task (retention={}d)", retention_days);

            match storage.find_expired_soft_deleted(retention_days) {
                Ok(expired_ids) => {
                    if expired_ids.is_empty() {
                        info!("Hard-purge: no expired chunks found");
                    } else {
                        let total = expired_ids.len();
                        let mut purged = 0usize;

                        for id in &expired_ids {
                            // Remove from graph
                            let _ = graph.remove_chunk_references(id);

                            // Remove from Tantivy
                            {
                                let mut t = tantivy.lock().await;
                                let _ = t.delete_document(id);
                            }

                            // Hard-delete from RocksDB
                            match storage.hard_delete_by_id(id) {
                                Ok(true) => purged += 1,
                                Ok(false) => warn!("Hard-purge: chunk {} not found", id),
                                Err(e) => error!("Hard-purge failed for {}: {}", id, e),
                            }
                        }

                        info!(
                            "Hard-purge completed: {}/{} chunks purged, duration={:.2}s",
                            purged,
                            total,
                            start.elapsed().as_secs_f64()
                        );
                        if let Some(w) = workers.get("purge") {
                            // truncation intentional: usize → u64 widens on 32-bit, identity on 64-bit
                            w.items_processed_total
                                .fetch_add(purged as u64, Ordering::SeqCst);
                        }
                    }
                    // Mark last_success_at on EVERY successful scan, not just
                    // when work was done — a healthy purge worker with nothing
                    // to purge during quiet retention windows must not appear
                    // "stuck" (cycle/68-critic MED#1).
                    if let Some(w) = workers.get("purge") {
                        w.last_success_at.store(now_epoch(), Ordering::SeqCst);
                    }
                }
                Err(e) => {
                    error!("Hard-purge: failed to find expired chunks: {}", e);
                }
            }

            running.store(false, Ordering::SeqCst);
        });
    }

    fn spawn_backup_task(&self) {
        let storage = self.storage.clone();
        let running = self.backup_running.clone();
        let data_dir = self.config.storage.data_dir.clone();
        let max_copies = self.config.worker.backup.max_copies;
        let workers = self.workers.clone();

        running.store(true, Ordering::SeqCst);

        tokio::spawn(async move {
            let start = std::time::Instant::now();

            if let Some(w) = workers.get("backup") {
                w.last_run_at.store(now_epoch(), Ordering::SeqCst);
            }

            info!("Starting backup task");

            let backup_dir = data_dir.join("backups");
            if let Err(e) = std::fs::create_dir_all(&backup_dir) {
                error!("Failed to create backup dir: {}", e);
                running.store(false, Ordering::SeqCst);
                return;
            }

            let date = chrono::Utc::now().format("%Y-%m-%d-%H%M%S").to_string();
            let dest = backup_dir.join(format!("checkpoint-{}", date));

            match storage.create_checkpoint(&dest) {
                Ok(()) => {
                    info!(
                        "Backup completed: {}, duration={:.2}s",
                        dest.display(),
                        start.elapsed().as_secs_f64()
                    );

                    apply_retention_policy(&backup_dir, max_copies);
                    if let Some(w) = workers.get("backup") {
                        w.items_processed_total.fetch_add(1, Ordering::SeqCst);
                        w.last_success_at.store(now_epoch(), Ordering::SeqCst);
                    }
                }
                Err(e) => {
                    error!("Backup failed: {}", e);
                }
            }

            running.store(false, Ordering::SeqCst);
        });
    }

    fn spawn_stats_task(&self) {
        let storage = self.storage.clone();
        let running = self.stats_running.clone();
        let events_dir = self
            .config
            .storage
            .data_dir
            .join(&self.config.event_log.dir);
        let workers = self.workers.clone();

        running.store(true, Ordering::SeqCst);

        tokio::spawn(async move {
            let start = std::time::Instant::now();

            if let Some(w) = workers.get("stats") {
                w.last_run_at.store(now_epoch(), Ordering::SeqCst);
            }

            info!("Starting stats aggregation task");

            match StatsAggregator::aggregate(&storage, &events_dir) {
                Ok(report) => {
                    info!(
                        "Stats aggregation completed: events={}, metrics={}, duration={:.2}s",
                        report.events_processed,
                        report.metrics_written,
                        start.elapsed().as_secs_f64()
                    );
                    if let Some(w) = workers.get("stats") {
                        w.items_processed_total.fetch_add(1, Ordering::SeqCst);
                        w.last_success_at.store(now_epoch(), Ordering::SeqCst);
                    }
                }
                Err(e) => {
                    error!("Stats aggregation failed: {}", e);
                }
            }

            running.store(false, Ordering::SeqCst);
        });
    }
}

/// Run one pass of ECA clustering + dream discovery across every active
/// stream discovered at runtime.
///
/// Stream discovery comes from `RocksDbStore::list_active_streams()`, which
/// scans `chunk:L0:` and `chunk:L1:` prefixes for distinct `chunk.stream`
/// values. This replaces an earlier hardcoded enumeration of
/// `config.namespaces.values()`, which only covered pre-multi-tenancy
/// fixed namespaces and never visited per-user `__user_<uuid>` streams.
///
/// Extracted from `spawn_clustering_task` — pure sync, no `.await`. Called via
/// `std::future::ready(...)` so `tokio::time::timeout` can wrap it uniformly.
/// Returns `(total_clusters, total_chunks)`.
///
/// IMPORTANT (cycle/75-critic LOW#4): if `cluster_stream` or `dream_discover`
/// ever become `async` (e.g. parallel stream processing via `try_join_all`),
/// this function MUST be restructured as `async fn` and the
/// `std::future::ready(run_clustering_pass(...))` wrap at the call site
/// MUST be replaced with a direct `run_clustering_pass(...).await`. The
/// current eager-evaluation form is correct only because the body is fully
/// synchronous — eager evaluation of an async body would block the spawn
/// task before `tokio::time::timeout` could even start measuring.
fn run_clustering_pass(
    storage: Arc<RocksDbStore>,
    config: crate::config::AssociatorConfig,
    graph: Arc<GraphStore>,
    event_tx: Option<EventSender>,
) -> (usize, usize) {
    let mut total_clusters = 0usize;
    let mut total_chunks = 0usize;

    let streams = match storage.list_active_streams() {
        Ok(s) => s,
        Err(e) => {
            warn!(
                "Clustering pass aborted — list_active_streams failed: {}",
                e
            );
            return (0, 0);
        }
    };

    info!("Clustering pass started for {} streams", streams.len());

    for stream_id in &streams {
        match crate::associator::clustering::cluster_stream(&storage, stream_id, &config) {
            Ok(result) => {
                info!(
                    "Clustering: stream {} processed {} chunks → {} clusters",
                    stream_id, result.num_chunks, result.num_clusters
                );
                total_clusters += result.num_clusters;
                total_chunks += result.num_chunks;
            }
            Err(e) => {
                warn!("Clustering failed for stream {}: {}", stream_id, e);
            }
        }
    }

    for stream_id in &streams {
        match crate::associator::dream::dream_discover(&storage, &graph, &config, stream_id) {
            Ok(report) => {
                if report.discoveries > 0 {
                    info!(
                        "Dream discovery for stream {}: {} discoveries from {} chunks in {}ms",
                        stream_id, report.discoveries, report.chunks_explored, report.duration_ms
                    );
                }

                // FIFO eviction (cap at 1000 per stream)
                let evictions =
                    crate::associator::dream::evict_old_latents(&storage, stream_id, 1000)
                        .unwrap_or(0);

                let latent_total = crate::associator::dream::count_latents(&storage, stream_id);

                if let Some(ref tx) = event_tx {
                    event_log::emit(
                        tx,
                        MemoryEvent::DreamCycle {
                            stream_id: stream_id.clone(),
                            discoveries: report.discoveries,
                            evictions,
                            latent_total,
                        },
                    );
                }
            }
            Err(e) => {
                warn!("Dream discovery failed for stream {}: {}", stream_id, e);
            }
        }
    }

    (total_clusters, total_chunks)
}

/// Retention policy: keep `max_daily` most-recent checkpoints plus 1 weekly
/// snapshot from a different ISO week. Deletes everything else.
fn apply_retention_policy(backup_dir: &std::path::Path, max_daily: usize) {
    let Ok(entries) = std::fs::read_dir(backup_dir) else {
        return;
    };

    let mut dirs: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir() && e.file_name().to_string_lossy().starts_with("checkpoint-"))
        .collect();

    dirs.sort_by_key(|e| std::cmp::Reverse(e.file_name()));

    let keep_daily: std::collections::HashSet<_> =
        dirs.iter().take(max_daily).map(|e| e.path()).collect();

    let newest_week = dirs
        .first()
        .and_then(|e| checkpoint_iso_week(e.file_name().to_string_lossy().as_ref()));

    let weekly_path = newest_week.and_then(|nw| {
        dirs.iter()
            .skip(max_daily)
            .find(|e| {
                checkpoint_iso_week(e.file_name().to_string_lossy().as_ref())
                    .is_some_and(|w| w != nw)
            })
            .map(|e| e.path())
    });

    for dir in &dirs {
        let path = dir.path();
        if keep_daily.contains(&path) {
            continue;
        }
        if weekly_path.as_ref() == Some(&path) {
            info!("Retaining weekly snapshot: {}", path.display());
            continue;
        }
        info!("Removing old backup: {}", path.display());
        let _ = std::fs::remove_dir_all(&path);
    }
}

/// Parses a checkpoint directory name and returns its ISO (year, week) pair.
/// Expected format: `checkpoint-YYYY-MM-DD-HHMMSS`.
fn checkpoint_iso_week(name: &str) -> Option<(i32, u32)> {
    use chrono::Datelike as _;
    let date_str = name.strip_prefix("checkpoint-")?;
    let mut parts = date_str.splitn(4, '-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    let d = chrono::NaiveDate::from_ymd_opt(year, month, day)?;
    let w = d.iso_week();
    Some((w.year(), w.week()))
}

#[cfg(test)]
mod clustering_pass_tests {
    use super::*;
    use crate::associator::AssociatorConfig;
    use crate::config::RocksDbConfig;
    use crate::storage::{Chunk, RocksDbStore};
    use tempfile::TempDir;

    fn rocks_test_config() -> RocksDbConfig {
        RocksDbConfig {
            max_open_files: 100,
            compression: "lz4".to_string(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        }
    }

    fn make_chunk(id: &str, stream: &str, level: i32) -> Chunk {
        Chunk {
            id: id.to_string(),
            content: format!("content {id}"),
            stream: stream.to_string(),
            level,
            score: 1.0,
            timestamp: 1000,
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

    /// Distinct embedding per chunk so kmeans actually has variance.
    fn seed_chunks(store: &RocksDbStore, stream: &str, prefix: &str) {
        const DIM: usize = 8;
        for i in 0..3 {
            let id = format!("{prefix}-{i}");
            let chunk = make_chunk(&id, stream, 1);
            store.store_chunk(&chunk).unwrap();
            let mut emb = vec![0.0f32; DIM];
            emb[i % DIM] = 1.0;
            emb[(i + 1) % DIM] = 0.5;
            store.store_embedding(&id, emb).unwrap();
        }
    }

    /// Regression for the fix: clustering visits user-uuid streams discovered
    /// at runtime, not just hardcoded `[namespaces]` stream IDs. The scheduler
    /// previously iterated `config.namespaces.values()`, which only covered
    /// pre-multi-tenancy fixed IDs and skipped every `__user_<uuid>` stream.
    #[test]
    fn run_clustering_pass_covers_user_streams_and_legacy_namespace() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(RocksDbStore::open(tmp.path(), &rocks_test_config()).unwrap());

        seed_chunks(&store, "__user_aaa", "a");
        seed_chunks(&store, "__user_bbb", "b");
        seed_chunks(&store, "100", "legacy");

        let config = AssociatorConfig {
            enabled: true,
            ..AssociatorConfig::default()
        };
        let graph = Arc::new(crate::graph::GraphStore::new(store.clone()));

        let (clusters, chunks) = run_clustering_pass(store.clone(), config, graph, None);

        assert_eq!(chunks, 9, "all 3 streams × 3 chunks should be processed");
        assert!(
            clusters >= 3,
            "expected ≥3 clusters across 3 streams (got {clusters})"
        );

        // Every seeded stream should have its assoc:cluster: metadata written.
        for id_prefix in ["a-0", "b-0", "legacy-0"] {
            let key = format!("assoc:cluster:{id_prefix}");
            assert!(
                store.get(key.as_bytes()).unwrap().is_some(),
                "missing cluster metadata for {id_prefix}"
            );
        }
    }

    #[test]
    fn run_clustering_pass_empty_storage_returns_zeros() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(RocksDbStore::open(tmp.path(), &rocks_test_config()).unwrap());
        let graph = Arc::new(crate::graph::GraphStore::new(store.clone()));

        let result = run_clustering_pass(store, AssociatorConfig::default(), graph, None);

        assert_eq!(result, (0, 0));
    }
}
