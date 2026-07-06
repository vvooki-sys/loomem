//! Stream-statistics endpoints: an inventory/health snapshot of a stream's
//! memory store (counts, level breakdown, retrieval readiness, fact-type /
//! attribution / trust-tier distributions, rolling activity, extraction
//! quality). Distinct from `handlers/stats.rs`, which reports retrieval-quality
//! metrics (hit rate, MRR, freshness).
//!
//! Privacy invariant: this endpoint MUST NOT return any chunk content — only
//! aggregates (counts, timestamps, histograms). The heavy lifting and the same
//! invariant live in `loomem_core::stream_stats`.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    Json,
};
use loomem_core::stream_stats::{self, AllStreamStats, ComputeOpts, StreamStats};
use serde::{Deserialize, Serialize};

use super::AppError;
use crate::auth::AuthContext;
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct StreamStatsQuery {
    /// Admin only: restrict to one stream. Omit to aggregate every stream.
    #[serde(default)]
    pub stream: Option<String>,
}

/// Admin response: either a single stream's stats (when `?stream=X` is given)
/// or every stream plus a `_total` aggregate.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum AdminStreamStatsResponse {
    One(Box<StreamStats>),
    All(Box<AllStreamStats>),
}

/// Build the compute inputs from config + clock (injected so the core stays
/// clock-free and testable).
fn opts_for(state: &AppState) -> ComputeOpts {
    ComputeOpts {
        now: u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0),
        min_chunks_to_consolidate: u64::try_from(
            state.config.worker.consolidation.min_chunks_to_consolidate,
        )
        .unwrap_or(0),
        event_log_enabled: state.config.event_log.enabled,
    }
}

fn events_dir(state: &AppState) -> std::path::PathBuf {
    state
        .config
        .storage
        .data_dir
        .join(&state.config.event_log.dir)
}

/// Fill the per-stream BM25 index count (needs the async Tantivy handle, so it
/// is done here rather than in the sync core). Best-effort: a failure leaves
/// the field `None`.
async fn fill_tantivy_one(state: &AppState, stats: &mut StreamStats) {
    let idx = state.tantivy.lock().await;
    stats.retrieval.tantivy_indexed_count = idx.count_stream(&stats.stream_id).ok();
}

async fn fill_tantivy_all(state: &AppState, all: &mut AllStreamStats) {
    let idx = state.tantivy.lock().await;
    for (id, s) in all.streams.iter_mut() {
        s.retrieval.tantivy_indexed_count = idx.count_stream(id).ok();
    }
    all.total.retrieval.tantivy_indexed_count = idx.count().ok();
}

/// GET /v1/my/stream-stats — full statistics for the caller's own stream.
pub async fn user_stream_stats_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Json<StreamStats>, AppError> {
    let opts = opts_for(&state);
    let dir = events_dir(&state);
    let mut stats = stream_stats::compute_stream(&state.store, &dir, &opts, &auth.stream_id)
        .map_err(AppError::Internal)?;
    fill_tantivy_one(&state, &mut stats).await;
    Ok(Json(stats))
}

/// GET /v1/admin/stream-stats?stream=X — one stream, or all streams + `_total`.
/// Admin token only (mirrors `/v1/status`).
pub async fn admin_stream_stats_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Query(q): Query<StreamStatsQuery>,
) -> Result<Json<AdminStreamStatsResponse>, AppError> {
    if !auth.is_admin {
        return Err(AppError::Forbidden("Admin access required".to_string()));
    }
    let opts = opts_for(&state);
    let dir = events_dir(&state);
    match q.stream {
        Some(stream) if !stream.is_empty() => {
            let mut stats = stream_stats::compute_stream(&state.store, &dir, &opts, &stream)
                .map_err(AppError::Internal)?;
            fill_tantivy_one(&state, &mut stats).await;
            Ok(Json(AdminStreamStatsResponse::One(Box::new(stats))))
        }
        _ => {
            let mut all =
                stream_stats::compute_all(&state.store, &dir, &opts).map_err(AppError::Internal)?;
            fill_tantivy_all(&state, &mut all).await;
            Ok(Json(AdminStreamStatsResponse::All(Box::new(all))))
        }
    }
}
