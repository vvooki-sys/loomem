//! Scheduled dream pass: one `dream_run` per active stream, aggregated.
//!
//! Lives next to `dream.rs` (which is at its file-size cap) and is driven by
//! `scheduler.rs::tick_dream`. The pass covers **all** active streams, the
//! shared stream included: the schedule is an instance-wide setting of a
//! single-user engine, and the owner who configures the instance also owns
//! the shared stream.
//!
//! `schedule_timeout_secs` is a *soft* deadline: once it has passed no new
//! stream is started, but the stream in progress always finishes (each LLM
//! call already carries its own 30 s timeout), so `dream_run` is never
//! cancelled half-way through a merge.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use reqwest::Client;
use tracing::{error, info, warn};

use crate::config::LlmConfig;
use crate::cost_tracker::CostTracker;
use crate::dream::{dream_run, DreamConfig, DreamResult, DreamRunContext};
use crate::dream_schedule::{self, DreamPassSummary, STATUS_DEADLINE, STATUS_FAILED, STATUS_OK};
use crate::embedding_queue::EmbeddingQueue;
use crate::intent_log::IntentLog;
use crate::pii_filter::PiiFilter;
use crate::storage::RocksDbStore;
use crate::tantivy_index::TantivyIndex;
use crate::workers_registry::{now_epoch, WorkerRegistry};

/// Borrowed inputs for one pass over all streams.
pub struct DreamPassContext<'a> {
    pub store: &'a RocksDbStore,
    pub tantivy: &'a tokio::sync::Mutex<TantivyIndex>,
    pub client: &'a Client,
    pub run_ctx: DreamRunContext<'a>,
    pub cost_tracker: &'a CostTracker,
    /// `"schedule"` or `"manual"`; copied into the summary.
    pub trigger: &'a str,
    /// Soft deadline: no new stream is started at or after this instant.
    /// `None` = unbounded. Injected so tests can pass an already-reached one.
    pub deadline: Option<Instant>,
}

/// `true` when `now` has reached the (optional) deadline.
pub fn deadline_reached(deadline: Option<Instant>, now: Instant) -> bool {
    deadline.is_some_and(|d| now >= d)
}

fn new_summary(trigger: &str, started_at: u64, streams_total: usize) -> DreamPassSummary {
    DreamPassSummary {
        started_at,
        finished_at: 0,
        streams_total,
        streams_failed: 0,
        chunks_processed: 0,
        facts_merged: 0,
        contradictions_resolved: 0,
        cost_usd: 0.0,
        cost_cap_reached: 0,
        streams_skipped_busy: 0,
        trigger: trigger.to_string(),
        status: STATUS_OK.to_string(),
    }
}

fn accumulate(summary: &mut DreamPassSummary, result: &DreamResult) {
    if result.skipped_busy {
        summary.streams_skipped_busy += 1;
        return;
    }
    summary.chunks_processed += result.chunks_processed;
    summary.facts_merged += result.facts_merged;
    summary.contradictions_resolved += result.contradictions_resolved;
    summary.cost_usd += result.cost_usd;
    if result.cost_cap_reached {
        summary.cost_cap_reached += 1;
    }
}

/// Run `dream_run` on every active stream (sorted, as `list_active_streams`
/// returns them). A failing stream is logged and counted in `streams_failed`
/// and the pass continues; a stream list that cannot be read yields a
/// summary with `status = "failed"`.
pub async fn dream_run_all_streams(ctx: DreamPassContext<'_>) -> DreamPassSummary {
    let started_at = now_epoch();
    let streams = match ctx.store.list_active_streams() {
        Ok(streams) => streams,
        Err(e) => {
            error!(
                trigger = ctx.trigger,
                "Dream pass: cannot list streams: {e:#}"
            );
            let mut failed = new_summary(ctx.trigger, started_at, 0);
            failed.status = STATUS_FAILED.to_string();
            failed.finished_at = now_epoch();
            return failed;
        }
    };
    let mut summary = new_summary(ctx.trigger, started_at, streams.len());

    for (visited, stream) in streams.iter().enumerate() {
        if deadline_reached(ctx.deadline, Instant::now()) {
            warn!(
                trigger = ctx.trigger,
                "Dream pass: soft deadline reached after {visited}/{} streams, not starting more",
                streams.len()
            );
            summary.status = STATUS_DEADLINE.to_string();
            break;
        }
        let run_ctx = DreamRunContext {
            pii_filter: ctx.run_ctx.pii_filter,
            llm_config: ctx.run_ctx.llm_config,
            dream_config: ctx.run_ctx.dream_config,
            intent_log: ctx.run_ctx.intent_log,
            embedding_queue: ctx.run_ctx.embedding_queue.clone(),
        };
        match dream_run(
            ctx.store,
            ctx.tantivy,
            ctx.client,
            run_ctx,
            ctx.cost_tracker,
            stream,
        )
        .await
        {
            Ok(result) => accumulate(&mut summary, &result),
            Err(e) => {
                summary.streams_failed += 1;
                warn!(trigger = ctx.trigger, "Dream pass: stream failed: {e:#}");
            }
        }
    }

    summary.finished_at = now_epoch();
    summary
}

/// Owned handles for a scheduled pass, cloned from the scheduler before
/// `tokio::spawn` so the task outlives the tick.
pub struct ScheduledPass {
    pub store: Arc<RocksDbStore>,
    pub tantivy: Arc<tokio::sync::Mutex<TantivyIndex>>,
    pub client: Client,
    pub llm_config: LlmConfig,
    pub dream_config: DreamConfig,
    pub pii_filter: Arc<PiiFilter>,
    pub cost_tracker: Arc<CostTracker>,
    pub intent_log: Option<Arc<tokio::sync::Mutex<IntentLog>>>,
    pub embedding_queue: Option<EmbeddingQueue>,
    pub workers: WorkerRegistry,
}

/// Start the pass on the runtime if no other is running: the `running`
/// flag is taken with `compare_exchange`, so of two concurrent callers only
/// one spawns. Returns `false` for the loser.
pub fn spawn_scheduled_pass(
    pass: ScheduledPass,
    running: Arc<AtomicBool>,
    trigger: &'static str,
) -> bool {
    if running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        warn!("Dream pass already running, not starting another");
        return false;
    }
    tokio::spawn(async move {
        run_scheduled_pass(pass, trigger).await;
        running.store(false, Ordering::SeqCst);
    });
    true
}

/// Persist the start marker (`last_run_at = now`, manual request consumed).
/// A pass may only start once this is durable: a crash mid-pass must not
/// re-fire in a loop, and a queued request must not be served twice.
pub fn mark_started(store: &RocksDbStore, started_at: u64) -> Result<()> {
    dream_schedule::update(store, |row| {
        row.last_run_at = Some(started_at);
        row.run_requested_at = None;
    })
    .map(drop)
}

/// Body of the scheduler's dream task: durable start marker first (skip the
/// pass if it cannot be written), then the pass under the soft deadline,
/// then the outcome. Every pass that started leaves a `last_result`.
pub async fn run_scheduled_pass(pass: ScheduledPass, trigger: &'static str) {
    let started_at = now_epoch();
    if let Err(e) = mark_started(&pass.store, started_at) {
        error!(
            trigger,
            "Dream pass skipped: cannot persist the start marker: {e:#}"
        );
        return;
    }
    if let Some(w) = pass.workers.get("dream") {
        w.last_run_at.store(started_at, Ordering::SeqCst);
    }
    info!(trigger, "Starting dream pass over all active streams");

    let deadline =
        Instant::now().checked_add(Duration::from_secs(pass.dream_config.schedule_timeout_secs));
    let summary = dream_run_all_streams(DreamPassContext {
        store: &pass.store,
        tantivy: &pass.tantivy,
        client: &pass.client,
        run_ctx: DreamRunContext {
            pii_filter: &pass.pii_filter,
            llm_config: &pass.llm_config,
            dream_config: &pass.dream_config,
            intent_log: pass.intent_log.as_deref(),
            embedding_queue: pass.embedding_queue.clone(),
        },
        cost_tracker: &pass.cost_tracker,
        trigger,
        deadline,
    })
    .await;
    record_outcome(&pass, summary);
}

fn record_outcome(pass: &ScheduledPass, summary: DreamPassSummary) {
    info!(
        trigger = summary.trigger.as_str(),
        status = summary.status.as_str(),
        "Dream pass finished: streams={} failed={} skipped_busy={} chunks={} merged={} contradictions={} cost=${:.4} cap_hits={}",
        summary.streams_total,
        summary.streams_failed,
        summary.streams_skipped_busy,
        summary.chunks_processed,
        summary.facts_merged,
        summary.contradictions_resolved,
        summary.cost_usd,
        summary.cost_cap_reached
    );
    if let Some(w) = pass.workers.get("dream") {
        let merged = u64::try_from(summary.facts_merged).unwrap_or(u64::MAX);
        w.items_processed_total.fetch_add(merged, Ordering::SeqCst);
        // `deadline` still did real work; only a pass that could not run at
        // all is withheld from `last_success_at`.
        if summary.status != STATUS_FAILED {
            w.last_success_at.store(now_epoch(), Ordering::SeqCst);
        }
    }
    if let Err(e) = dream_schedule::update(&pass.store, |row| row.last_result = Some(summary)) {
        error!("Dream pass: cannot write last_result: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PiiConfig;
    use crate::cost_tracker::CostConfig;
    use crate::scheduler::WorkerConfig;
    use crate::storage::RocksDbConfig;
    use crate::tantivy_index::TantivyConfig;
    use crate::workers_registry::build_registry;
    use tempfile::TempDir;

    fn scheduled_pass(tmp: &TempDir) -> ScheduledPass {
        let store = Arc::new(
            RocksDbStore::open(
                tmp.path(),
                &RocksDbConfig {
                    max_open_files: 100,
                    compression: "none".to_string(),
                    write_buffer_size: 4 * 1024 * 1024,
                    max_write_buffer_number: 2,
                },
            )
            .expect("open tempdir store"),
        );
        let tantivy = TantivyIndex::open(
            tmp.path().join("tantivy"),
            &TantivyConfig {
                enabled: false,
                heap_size_mb: 16,
                drift_warn_pct: 5.0,
                auto_rebuild_on_drift: false,
            },
        )
        .expect("open tantivy");
        let client = Client::new();
        ScheduledPass {
            cost_tracker: Arc::new(CostTracker::new(
                store.clone(),
                CostConfig::default(),
                client.clone(),
            )),
            store,
            tantivy: Arc::new(tokio::sync::Mutex::new(tantivy)),
            client,
            llm_config: LlmConfig::default(),
            dream_config: DreamConfig::default(),
            pii_filter: Arc::new(PiiFilter::new(PiiConfig::default()).expect("pii filter")),
            intent_log: None,
            embedding_queue: None,
            workers: build_registry(&WorkerConfig::default(), 86_400, 3_600, 86_400),
        }
    }

    #[test]
    fn deadline_reached_only_at_or_after_the_instant() {
        let now = Instant::now();
        assert!(!deadline_reached(None, now));
        assert!(deadline_reached(Some(now), now));
        assert!(deadline_reached(Some(now), now + Duration::from_millis(1)));
        assert!(!deadline_reached(Some(now + Duration::from_secs(60)), now));
    }

    #[test]
    fn accumulate_counts_a_busy_stream_as_skipped_not_processed() {
        let mut summary = new_summary(dream_schedule::TRIGGER_SCHEDULE, 1, 2);
        accumulate(&mut summary, &DreamResult::skipped_busy("busy"));
        assert_eq!(summary.streams_skipped_busy, 1);
        assert_eq!(summary.chunks_processed, 0);
        assert_eq!(summary.cost_cap_reached, 0);
    }

    /// Only the caller that flips `running` spawns; the flag is released
    /// again once the (empty-store) pass has finished.
    #[tokio::test]
    async fn spawn_scheduled_pass_runs_once_per_running_flag() {
        let tmp = TempDir::new().expect("tempdir");
        let pass = scheduled_pass(&tmp);
        let store = pass.store.clone();
        let running = Arc::new(AtomicBool::new(true));
        // The losing pass needs its own store: RocksDB locks the directory.
        let other = TempDir::new().expect("tempdir for the losing pass");
        assert!(
            !spawn_scheduled_pass(scheduled_pass(&other), running.clone(), "manual"),
            "a flag already set means another pass owns it"
        );
        running.store(false, Ordering::SeqCst);
        assert!(spawn_scheduled_pass(pass, running.clone(), "manual"));
        // Deterministic wait: the pass is a spawned task on this runtime and
        // the store is empty, so it finishes as soon as it is polled.
        while running.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        assert!(dream_schedule::load(&store)
            .expect("row")
            .last_result
            .is_some());
    }

    #[test]
    fn mark_started_sets_last_run_and_clears_request() -> Result<()> {
        let tmp = TempDir::new()?;
        let pass = scheduled_pass(&tmp);
        dream_schedule::update(&pass.store, |row| row.run_requested_at = Some(5))?;
        mark_started(&pass.store, 1_000)?;
        let row = dream_schedule::load(&pass.store)?;
        assert_eq!(row.last_run_at, Some(1_000));
        assert_eq!(row.run_requested_at, None);
        Ok(())
    }

    /// Empty store → zero streams → no LLM call; the row still records the
    /// start marker and an `ok` summary.
    #[tokio::test]
    async fn run_scheduled_pass_on_empty_store_records_ok_summary() {
        let tmp = TempDir::new().expect("tempdir");
        let pass = scheduled_pass(&tmp);
        dream_schedule::update(&pass.store, |row| row.run_requested_at = Some(7))
            .expect("queue a manual run");
        let store = pass.store.clone();
        let workers = pass.workers.clone();

        run_scheduled_pass(pass, dream_schedule::TRIGGER_MANUAL).await;

        let row = dream_schedule::load(&store).expect("row");
        assert!(row.last_run_at.is_some(), "start marker persisted");
        assert_eq!(row.run_requested_at, None, "manual request consumed");
        let result = row.last_result.expect("summary recorded");
        assert_eq!(result.status, STATUS_OK);
        assert_eq!(result.streams_total, 0);
        assert_eq!(result.streams_failed, 0);
        assert_eq!(result.streams_skipped_busy, 0);
        assert_eq!(result.trigger, dream_schedule::TRIGGER_MANUAL);
        assert!(result.finished_at >= result.started_at);

        let w = workers.get("dream").expect("dream worker registered");
        assert_ne!(w.last_run_at.load(Ordering::SeqCst), 0);
        assert_ne!(w.last_success_at.load(Ordering::SeqCst), 0);
        assert_eq!(w.items_processed_total.load(Ordering::SeqCst), 0);
    }

    /// A deadline that has already passed before the first stream leaves an
    /// empty store with `ok` (nothing was skipped) — the deadline only
    /// matters once there is a stream to skip.
    #[tokio::test]
    async fn all_streams_pass_with_reached_deadline_on_empty_store_is_ok() {
        let tmp = TempDir::new().expect("tempdir");
        let pass = scheduled_pass(&tmp);
        let summary = dream_run_all_streams(DreamPassContext {
            store: &pass.store,
            tantivy: &pass.tantivy,
            client: &pass.client,
            run_ctx: DreamRunContext {
                pii_filter: &pass.pii_filter,
                llm_config: &pass.llm_config,
                dream_config: &pass.dream_config,
                intent_log: None,
                embedding_queue: None,
            },
            cost_tracker: &pass.cost_tracker,
            trigger: dream_schedule::TRIGGER_SCHEDULE,
            deadline: Some(Instant::now()),
        })
        .await;
        assert_eq!(summary.status, STATUS_OK);
        assert_eq!(summary.streams_total, 0);
    }
}
