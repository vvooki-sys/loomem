//! Scheduled dream pass: one `dream_run` per active stream, aggregated.
//!
//! Lives next to `dream.rs` (which is at its file-size cap) and is driven by
//! `scheduler.rs::tick_dream`. The pass covers **all** active streams, the
//! shared stream included: the schedule is edited only by an admin in the
//! dashboard, which satisfies the human gate that `memory_dream` enforces for
//! non-admin MCP callers.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use reqwest::Client;
use tracing::{error, info, warn};

use crate::config::LlmConfig;
use crate::cost_tracker::CostTracker;
use crate::dream::{dream_run, DreamConfig, DreamRunContext};
use crate::dream_schedule::{self, DreamPassSummary, DreamScheduleRow};
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
}

/// Run `dream_run` on every active stream (sorted, as `list_active_streams`
/// returns them). A failing stream is logged and counted in `streams_failed`;
/// the pass continues. `Err` only when the stream list itself cannot be read.
pub async fn dream_run_all_streams(ctx: DreamPassContext<'_>) -> Result<DreamPassSummary> {
    let streams = ctx.store.list_active_streams()?;
    let mut summary = DreamPassSummary {
        started_at: now_epoch(),
        finished_at: 0,
        streams_total: streams.len(),
        streams_failed: 0,
        chunks_processed: 0,
        facts_merged: 0,
        contradictions_resolved: 0,
        cost_usd: 0.0,
        cost_cap_reached: 0,
        trigger: ctx.trigger.to_string(),
    };

    for stream in &streams {
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
            Ok(result) => {
                summary.chunks_processed += result.chunks_processed;
                summary.facts_merged += result.facts_merged;
                summary.contradictions_resolved += result.contradictions_resolved;
                summary.cost_usd += result.cost_usd;
                if result.cost_cap_reached {
                    summary.cost_cap_reached += 1;
                }
            }
            Err(e) => {
                summary.streams_failed += 1;
                warn!(trigger = ctx.trigger, "Dream pass: stream failed: {e:#}");
            }
        }
    }

    summary.finished_at = now_epoch();
    Ok(summary)
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

/// Body of the scheduler's dream task: mark the start in the schedule row
/// (so a crash mid-pass cannot re-fire in a loop), run the pass under
/// `schedule_timeout_secs`, then record the outcome.
pub async fn run_scheduled_pass(pass: ScheduledPass, trigger: &'static str) {
    let started_at = now_epoch();
    if let Some(w) = pass.workers.get("dream") {
        w.last_run_at.store(started_at, Ordering::SeqCst);
    }
    update_row(&pass.store, |row| {
        row.last_run_at = Some(started_at);
        row.run_requested_at = None;
    });
    info!(trigger, "Starting dream pass over all active streams");

    let timeout = Duration::from_secs(pass.dream_config.schedule_timeout_secs);
    let outcome = tokio::time::timeout(
        timeout,
        dream_run_all_streams(DreamPassContext {
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
        }),
    )
    .await;

    match outcome {
        Ok(Ok(summary)) => record_success(&pass, summary),
        Ok(Err(e)) => error!(trigger, "Dream pass failed: {e:#}"),
        Err(_) => error!(
            trigger,
            "Dream pass timed out after {}s (dream.schedule_timeout_secs)",
            timeout.as_secs()
        ),
    }
}

fn record_success(pass: &ScheduledPass, summary: DreamPassSummary) {
    info!(
        trigger = summary.trigger.as_str(),
        "Dream pass completed: streams={} failed={} chunks={} merged={} contradictions={} cost=${:.4} cap_hits={}",
        summary.streams_total,
        summary.streams_failed,
        summary.chunks_processed,
        summary.facts_merged,
        summary.contradictions_resolved,
        summary.cost_usd,
        summary.cost_cap_reached
    );
    if let Some(w) = pass.workers.get("dream") {
        let merged = u64::try_from(summary.facts_merged).unwrap_or(u64::MAX);
        w.items_processed_total.fetch_add(merged, Ordering::SeqCst);
        w.last_success_at.store(now_epoch(), Ordering::SeqCst);
    }
    update_row(&pass.store, |row| row.last_result = Some(summary));
}

/// Read-modify-write of the schedule row. Re-reads before writing so an
/// admin `PUT` made while the pass ran is not overwritten by stale fields.
fn update_row(store: &RocksDbStore, mutate: impl FnOnce(&mut DreamScheduleRow)) {
    let mut row = match dream_schedule::load(store) {
        Ok(row) => row,
        Err(e) => {
            error!("Dream pass: cannot read schedule row, state not updated: {e:#}");
            return;
        }
    };
    mutate(&mut row);
    if let Err(e) = dream_schedule::save(store, &row) {
        error!("Dream pass: cannot write schedule row: {e:#}");
    }
}
