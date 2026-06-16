use axum::{
    extract::{Path, Query, State},
    Json,
};
use loomem_core::stats_aggregator::{
    FeedbackRecord, FreshnessCurve, MemoryProfile, ScoreDistribution, StatsAggregator, StatsSummary,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::AppError;
use crate::auth::AuthContext;
use crate::AppState;

// ---- Response types ----

#[derive(Debug, Serialize)]
pub struct StatsSummaryResponse {
    pub stream_id: String,
    pub search_count: u64,
    pub store_count: u64,
    pub mean_top1_score: f64,
    pub zero_result_rate: f64,
    pub hit_rate: f64,
    pub consolidation_count: u64,
    pub consolidation_cost_usd: f64,
    pub mrr: f64,
    pub score_distribution: ScoreDistribution,
    pub freshness_curve: FreshnessCurve,
    pub recall_ratio: f64,
    pub dead_memory_pct: f64,
    pub hot_memory_ids: Vec<String>,
    pub regret_rate: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mechanism_stats: Option<loomem_core::associator::tracking::MechanismStats>,
}

impl StatsSummaryResponse {
    fn from_summary(stream_id: String, s: StatsSummary) -> Self {
        Self {
            stream_id,
            search_count: s.search_count,
            store_count: s.store_count,
            mean_top1_score: s.mean_top1_score,
            zero_result_rate: s.zero_result_rate,
            hit_rate: s.hit_rate,
            consolidation_count: s.consolidation_count,
            consolidation_cost_usd: s.consolidation_cost_usd,
            mrr: s.mrr,
            score_distribution: s.score_distribution,
            freshness_curve: s.freshness_curve,
            recall_ratio: s.recall_ratio,
            dead_memory_pct: s.dead_memory_pct,
            hot_memory_ids: s.hot_memory_ids,
            regret_rate: s.regret_rate,
            mechanism_stats: s.mechanism_stats,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct TrendsParams {
    pub metric: String,
    #[serde(default = "default_period")]
    pub period: String,
}

fn default_period() -> String {
    "7d".to_string()
}

#[derive(Debug, Serialize)]
pub struct TrendsResponse {
    pub stream_id: String,
    pub metric: String,
    pub period: String,
    pub data: Vec<TrendPoint>,
}

#[derive(Debug, Serialize)]
pub struct TrendPoint {
    pub timestamp: u64,
    pub value: f64,
}

#[derive(Debug, Deserialize)]
pub struct FeedbackRequest {
    pub chunk_id: String,
    pub useful: bool,
    #[serde(default)]
    pub rank_position: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct FeedbackResponse {
    pub ok: bool,
}

// ---- Handlers ----

/// GET /v1/stats/summary - all latest metrics for the auth'd stream
pub async fn stats_summary_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Json<StatsSummaryResponse>, AppError> {
    let stream_id = &auth.stream_id;
    let summary = StatsAggregator::get_summary(&state.store, stream_id)?;
    Ok(Json(StatsSummaryResponse::from_summary(
        stream_id.clone(),
        summary,
    )))
}

/// GET /v1/stats/stream/:id - per-stream metrics (admin or own stream)
pub async fn stats_stream_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(stream_id): Path<String>,
) -> Result<Json<StatsSummaryResponse>, AppError> {
    // Allow access to own stream, or admin can see any stream
    if stream_id != auth.stream_id && !auth.is_admin {
        return Err(AppError::BadRequest(
            "Access denied: can only view own stream stats".to_string(),
        ));
    }
    let summary = StatsAggregator::get_summary(&state.store, &stream_id)?;
    Ok(Json(StatsSummaryResponse::from_summary(stream_id, summary)))
}

/// GET /v1/stats/trends?metric=hit_rate&period=7d - time series
pub async fn stats_trends_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Query(params): Query<TrendsParams>,
) -> Result<Json<TrendsResponse>, AppError> {
    let stream_id = &auth.stream_id;

    // Parse period: "7d", "24h", "30d", etc.
    let hours = parse_period_to_hours(&params.period).map_err(AppError::BadRequest)?;

    // Validate metric name
    let valid_metrics = [
        "search_count",
        "store_count",
        "mean_top1_score",
        "zero_result_rate",
        "hit_rate",
        "consolidation_count",
        "consolidation_cost_usd",
        "mrr",
    ];
    if !valid_metrics.contains(&params.metric.as_str()) {
        return Err(AppError::BadRequest(format!(
            "Invalid metric '{}'. Valid metrics: {}",
            params.metric,
            valid_metrics.join(", ")
        )));
    }

    let trend = StatsAggregator::get_trend(&state.store, stream_id, &params.metric, hours)?;
    let data: Vec<TrendPoint> = trend
        .into_iter()
        .map(|(ts, val)| TrendPoint {
            timestamp: ts,
            value: val,
        })
        .collect();

    Ok(Json(TrendsResponse {
        stream_id: stream_id.clone(),
        metric: params.metric,
        period: params.period,
        data,
    }))
}

/// POST /v1/stats/feedback - mark chunk as useful/not useful
pub async fn stats_feedback_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Json(payload): Json<FeedbackRequest>,
) -> Result<Json<FeedbackResponse>, AppError> {
    let now = chrono::Utc::now().timestamp() as u64;

    let record = FeedbackRecord {
        chunk_id: payload.chunk_id,
        stream_id: auth.stream_id.clone(),
        useful: payload.useful,
        rank_position: payload.rank_position,
        timestamp: now,
    };

    StatsAggregator::store_feedback(&state.store, &record)?;

    Ok(Json(FeedbackResponse { ok: true }))
}

// ---- ECA-12: Advisory endpoint ----

#[derive(Debug, Deserialize)]
pub struct AdvisoryParams {
    pub stream_id: Option<String>,
    #[serde(default = "default_advisory_limit")]
    pub limit: usize,
    /// If true, run full detection (slower). Default: false (use cached advisories).
    #[serde(default)]
    pub refresh: bool,
}

fn default_advisory_limit() -> usize {
    10
}

#[derive(Debug, Serialize)]
pub struct AdvisoryResponse {
    pub stream_id: String,
    pub advisories: Vec<loomem_core::advisor::AdvisoryItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decay_adjustment: Option<f64>,
}

/// GET /v1/advisory?stream_id=100&limit=10&refresh=false
pub async fn advisory_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Query(params): Query<AdvisoryParams>,
) -> Result<Json<AdvisoryResponse>, AppError> {
    let stream_id = params.stream_id.unwrap_or_else(|| auth.stream_id.clone());

    // Access control
    if stream_id != auth.stream_id && !auth.is_admin {
        return Err(AppError::BadRequest(
            "Access denied: can only view own stream advisories".to_string(),
        ));
    }

    let limit = params.limit.min(50);

    let mut advisories = if params.refresh {
        // Full detection pass
        let cfg = &state.config.advisor;
        let events_dir = state
            .config
            .storage
            .data_dir
            .join(&state.config.event_log.dir);
        loomem_core::advisor::detect_advisories_with_config(
            &state.store,
            &events_dir,
            &stream_id,
            cfg.repeated_query_threshold,
            cfg.stale_fact_days,
            limit,
        )?
    } else {
        // Lightweight: read cached advisories from RocksDB
        loomem_core::advisor::get_cached_advisories(&state.store, &stream_id, limit)
    };

    // When refresh=true OR cached advisories are empty, generate actionable advisories
    if params.refresh || advisories.is_empty() {
        if let Ok(actionable) =
            loomem_core::advisor::generate_actionable_advisories(&state.store, &stream_id)
        {
            advisories.extend(actionable);
            advisories.truncate(limit);
        }
    }

    // ECA-14: Compute decay adjustment suggestion
    let decay_adjustment = loomem_core::advisor::compute_decay_adjustment(&state.store, &stream_id)
        .ok()
        .flatten();

    Ok(Json(AdvisoryResponse {
        stream_id,
        advisories,
        decay_adjustment,
    }))
}

// ---- ECA-24: Advisory outcome tracking ----

#[derive(Debug, Deserialize)]
pub struct AdvisoryOutcomeRequest {
    pub advisory_id: String,
    pub followed: bool,
}

#[derive(Debug, Serialize)]
pub struct AdvisoryOutcomeResponse {
    pub ok: bool,
}

/// POST /v1/advisory/outcome - track that an advisory was followed or dismissed
pub async fn advisory_outcome_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(_auth): axum::Extension<AuthContext>,
    Json(payload): Json<AdvisoryOutcomeRequest>,
) -> Result<Json<AdvisoryOutcomeResponse>, AppError> {
    loomem_core::advisor::track_advisory_outcome(
        &state.store,
        &payload.advisory_id,
        payload.followed,
    )?;
    Ok(Json(AdvisoryOutcomeResponse { ok: true }))
}

// ---- ECA-25: Advisory effectiveness ----

/// GET /v1/advisory/effectiveness - get advisory effectiveness stats
pub async fn advisory_effectiveness_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Json<loomem_core::advisor::AdvisoryEffectiveness>, AppError> {
    let effectiveness =
        loomem_core::advisor::get_advisory_effectiveness(&state.store, &auth.stream_id)?;
    Ok(Json(effectiveness))
}

/// POST /v1/advisory/adjust-weights - adjust advisory weights based on follow rates
pub async fn advisory_adjust_weights_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Json<Vec<loomem_core::advisor::WeightAdjustment>>, AppError> {
    let adjustments = loomem_core::advisor::adjust_advisory_weights(&state.store, &auth.stream_id)?;
    Ok(Json(adjustments))
}

// ---- ECA-26: Per-stream memory profile ----

/// GET /v1/stats/stream/:id/profile - per-stream memory profile
pub async fn stats_profile_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(stream_id): Path<String>,
) -> Result<Json<MemoryProfile>, AppError> {
    if stream_id != auth.stream_id && !auth.is_admin {
        return Err(AppError::BadRequest(
            "Access denied: can only view own stream profile".to_string(),
        ));
    }
    let events_dir = state
        .config
        .storage
        .data_dir
        .join(&state.config.event_log.dir);
    let profile = MemoryProfile::compute(&state.store, &events_dir, &stream_id)?;
    Ok(Json(profile))
}

// ---- ECA-27: Association tracking ----

#[derive(Debug, Deserialize)]
pub struct AssocConsumedRequest {
    pub chunk_id: String,
    pub mechanism: String,
}

#[derive(Debug, Serialize)]
pub struct AssocConsumedResponse {
    pub ok: bool,
}

/// POST /v1/associations/consumed - track that an association was consumed
pub async fn assoc_consumed_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(_auth): axum::Extension<AuthContext>,
    Json(payload): Json<AssocConsumedRequest>,
) -> Result<Json<AssocConsumedResponse>, AppError> {
    loomem_core::associator::tracking::track_association_consumed(
        &state.store,
        &payload.chunk_id,
        &payload.mechanism,
    )?;
    Ok(Json(AssocConsumedResponse { ok: true }))
}

// ---- ECA-28: Dream discoveries endpoint ----

#[derive(Debug, Deserialize)]
pub struct DreamDiscoveriesParams {
    pub stream_id: Option<String>,
    #[serde(default = "default_dream_limit")]
    pub limit: usize,
}

fn default_dream_limit() -> usize {
    10
}

#[derive(Debug, Serialize)]
pub struct DreamDiscoveriesResponse {
    pub stream_id: String,
    pub discoveries: Vec<loomem_core::associator::dream::LatentAssociation>,
    pub stats: loomem_core::associator::dream::DreamStats,
}

/// GET /v1/dream/discoveries?stream_id=100&limit=10
pub async fn dream_discoveries_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Query(params): Query<DreamDiscoveriesParams>,
) -> Result<Json<DreamDiscoveriesResponse>, AppError> {
    let stream_id = params.stream_id.unwrap_or_else(|| auth.stream_id.clone());

    if stream_id != auth.stream_id && !auth.is_admin {
        return Err(AppError::BadRequest(
            "Access denied: can only view own stream dream discoveries".to_string(),
        ));
    }

    let limit = params.limit.min(50);
    let discoveries = loomem_core::associator::dream::get_unpromoted_latent_associations(
        &state.store,
        &stream_id,
        limit,
    )?;
    let stats = loomem_core::associator::dream::dream_stats(&state.store, &stream_id);

    Ok(Json(DreamDiscoveriesResponse {
        stream_id,
        discoveries,
        stats,
    }))
}

// ---- ECA: Dream trigger endpoint ----

/// POST /v1/dream/trigger - run dream discovery and return report
pub async fn dream_trigger_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Json<serde_json::Value>, AppError> {
    let stream_id = &auth.stream_id;
    let report = loomem_core::associator::dream::dream_discover(
        &state.store,
        &state.graph,
        &state.config.associator,
        stream_id,
    )?;

    Ok(Json(serde_json::json!({
        "stream_id": stream_id,
        "discoveries": report.discoveries,
        "chunks_explored": report.chunks_explored,
        "duration_ms": report.duration_ms,
    })))
}

// ---- helpers ----

fn parse_period_to_hours(period: &str) -> Result<usize, String> {
    let period = period.trim().to_lowercase();
    if let Some(days) = period.strip_suffix('d') {
        days.parse::<usize>()
            .map(|d| d * 24)
            .map_err(|_| format!("Invalid period: {}", period))
    } else if let Some(hours) = period.strip_suffix('h') {
        hours
            .parse::<usize>()
            .map_err(|_| format!("Invalid period: {}", period))
    } else {
        Err(format!(
            "Invalid period format '{}'. Use Nd (days) or Nh (hours), e.g. '7d' or '24h'",
            period
        ))
    }
}
