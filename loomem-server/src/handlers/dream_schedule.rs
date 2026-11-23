//! `/v1/dream/schedule` — admin-only view and control of the scheduled dream
//! pass. State lives in the `meta:dream_schedule` row; this module reads and
//! writes it only through `loomem_core::dream_schedule` (layering rule; every
//! write goes through `dream_schedule::update`) and never runs a pass itself:
//! the scheduler executes queued runs at its next poll, so manual and
//! scheduled passes share one executor.

use axum::extract::{FromRequest, State};
use axum::http::StatusCode;
use axum::Json;
use loomem_core::dream_schedule::{self, DreamScheduleRow};
use loomem_core::scheduler::DREAM_POLL_SECS;
use loomem_core::workers_registry::{is_worker_paused, now_epoch};
use std::sync::Arc;

use super::admin::require_admin;
use super::types::{DreamScheduleResponse, DreamScheduleUpdateRequest};
use super::AppError;
use crate::AppState;

fn response_from(state: &AppState, row: &DreamScheduleRow) -> DreamScheduleResponse {
    let eff = dream_schedule::effective(row, &state.config.dream);
    DreamScheduleResponse {
        dream_enabled: state.config.dream.enabled,
        scheduler_enabled: state.config.scheduler.enabled,
        enabled: eff.enabled,
        interval_secs: eff.interval_secs,
        source: eff.source,
        last_run_at: row.last_run_at,
        next_run_at: dream_schedule::next_run_at(row, &eff),
        run_requested_at: row.run_requested_at,
        poll_secs: DREAM_POLL_SECS,
        queued: row.run_requested_at.is_some(),
        paused: is_worker_paused(&state.workers, "dream"),
        running: dream_schedule::is_running(row),
        last_result: row.last_result.clone(),
    }
}

/// GET /v1/dream/schedule
pub async fn dream_schedule_get_handler(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Result<Json<DreamScheduleResponse>, AppError> {
    require_admin(&request)?;
    let row = dream_schedule::load(&state.store)?;
    Ok(Json(response_from(&state, &row)))
}

/// PUT /v1/dream/schedule — body `{ "enabled": bool, "interval_secs": u64 }`.
/// 400 when the interval is outside 1 h ..= 30 d. A disabled → enabled
/// transition re-anchors the schedule (`next_run_at = now + interval`).
pub async fn dream_schedule_put_handler(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Result<Json<DreamScheduleResponse>, AppError> {
    require_admin(&request)?;
    let Json(payload) = Json::<DreamScheduleUpdateRequest>::from_request(request, &())
        .await
        .map_err(|e| AppError::BadRequest(e.body_text()))?;
    if !dream_schedule::interval_in_bounds(payload.interval_secs) {
        return Err(AppError::BadRequest(format!(
            "interval_secs must be between {} and {} seconds (1 h .. 30 d)",
            dream_schedule::MIN_INTERVAL_SECS,
            dream_schedule::MAX_INTERVAL_SECS
        )));
    }

    let now = now_epoch();
    let row = dream_schedule::update(&state.store, |row| {
        dream_schedule::apply_override(
            row,
            &state.config.dream,
            payload.enabled,
            payload.interval_secs,
            now,
        );
    })?;
    Ok(Json(response_from(&state, &row)))
}

/// POST /v1/dream/schedule/run — queue a pass for the scheduler's next poll.
/// 409 when the dream worker or the scheduler is disabled in config, or the
/// `dream` worker is paused (nobody would execute the request).
pub async fn dream_schedule_run_handler(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Result<(StatusCode, Json<DreamScheduleResponse>), AppError> {
    require_admin(&request)?;
    if !state.config.dream.enabled {
        return Err(AppError::Conflict(
            "Dream worker is disabled in config ([dream] enabled = false)".to_string(),
        ));
    }
    if !state.config.scheduler.enabled {
        return Err(AppError::Conflict(
            "Scheduler is disabled in config ([scheduler] enabled = false)".to_string(),
        ));
    }
    if is_worker_paused(&state.workers, "dream") {
        return Err(AppError::Conflict("Dream worker is paused".to_string()));
    }

    let now = now_epoch();
    let row = dream_schedule::update(&state.store, |row| row.run_requested_at = Some(now))?;
    Ok((StatusCode::ACCEPTED, Json(response_from(&state, &row))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthContext, KeyScope};
    use loomem_core::storage::UserRole;

    fn non_admin_request() -> axum::extract::Request {
        let mut req = axum::extract::Request::new(axum::body::Body::empty());
        let ctx =
            AuthContext::single_stream("alice", UserRole::Writer, KeyScope::Private, None, false);
        req.extensions_mut().insert(ctx);
        req
    }

    /// The HTTP fixture only mints an admin token, so the 403 path is proven
    /// at the handler level with a non-admin `AuthContext`.
    #[tokio::test]
    async fn non_admin_is_forbidden_on_all_three_handlers() {
        let (_app, state) = crate::tests::make_test_app();

        let get = dream_schedule_get_handler(State(state.clone()), non_admin_request()).await;
        assert!(matches!(get, Err(AppError::Forbidden(_))));

        let put = dream_schedule_put_handler(State(state.clone()), non_admin_request()).await;
        assert!(matches!(put, Err(AppError::Forbidden(_))));

        let run = dream_schedule_run_handler(State(state), non_admin_request()).await;
        assert!(matches!(run, Err(AppError::Forbidden(_))));
    }
}
