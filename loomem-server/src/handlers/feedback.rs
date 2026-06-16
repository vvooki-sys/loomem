//! Cycle/112: `POST /v1/feedback` — agents return graded usefulness/harmful
//! signals for chunks they retrieved. v1 is write-only: schema + storage +
//! service layer; retrieval is unchanged.

use std::sync::Arc;

use axum::{extract::State, Json};
use loomem_core::feedback::{ApplyRatingArgs, FeedbackService, RatingOutcome};
use serde::{Deserialize, Serialize};

use super::AppError;
use crate::auth::AuthContext;
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct FeedbackRatingItem {
    pub chunk_id: String,
    pub usefulness: u8,
    pub harmful: bool,
    pub justification: String,
}

#[derive(Debug, Deserialize)]
pub struct FeedbackRequest {
    pub model_version: String,
    pub prompt_version: String,
    #[serde(default)]
    pub trajectory_id: Option<String>,
    pub ratings: Vec<FeedbackRatingItem>,
}

#[derive(Debug, Serialize)]
pub struct RejectedRating {
    pub chunk_id: String,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct FeedbackResponse {
    pub ok: bool,
    pub accepted: u32,
    pub rejected: Vec<RejectedRating>,
}

/// POST /v1/feedback
pub async fn feedback_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Json(payload): Json<FeedbackRequest>,
) -> Result<Json<FeedbackResponse>, AppError> {
    let cfg = &state.config.feedback;
    let svc = FeedbackService::new(&state.store, cfg);

    check_request_size(&payload, cfg.max_ratings_per_request)?;
    for r in &payload.ratings {
        svc.validate_rating(r.usefulness, r.harmful, &r.justification)
            .map_err(AppError::BadRequest)?;
    }

    let now_ms = chrono::Utc::now().timestamp_millis();
    let (accepted, rejected) = apply_all(&svc, &auth, &payload, now_ms)?;

    if accepted == 0 {
        return Err(AppError::BadRequest(
            "all ratings rejected (no chunk found in caller's stream)".to_string(),
        ));
    }

    Ok(Json(FeedbackResponse {
        ok: true,
        accepted,
        rejected,
    }))
}

fn check_request_size(payload: &FeedbackRequest, max: usize) -> Result<(), AppError> {
    if payload.ratings.is_empty() {
        return Err(AppError::BadRequest(
            "ratings must not be empty".to_string(),
        ));
    }
    if payload.ratings.len() > max {
        return Err(AppError::PayloadTooLarge(format!(
            "ratings length {} exceeds max_ratings_per_request={}",
            payload.ratings.len(),
            max
        )));
    }
    Ok(())
}

fn apply_all(
    svc: &FeedbackService<'_>,
    auth: &AuthContext,
    payload: &FeedbackRequest,
    now_ms: i64,
) -> Result<(u32, Vec<RejectedRating>), AppError> {
    let mut accepted: u32 = 0;
    let mut rejected: Vec<RejectedRating> = Vec::new();
    for r in &payload.ratings {
        let event_id = uuid::Uuid::new_v4().to_string();
        let args = build_args(r, auth, payload, now_ms, &event_id);
        match svc.apply_rating(args)? {
            RatingOutcome::Accepted => accepted = accepted.saturating_add(1),
            RatingOutcome::Rejected { chunk_id, reason } => {
                rejected.push(RejectedRating { chunk_id, reason });
            }
        }
    }
    Ok((accepted, rejected))
}

fn build_args<'a>(
    r: &'a FeedbackRatingItem,
    auth: &'a AuthContext,
    payload: &'a FeedbackRequest,
    now_ms: i64,
    event_id: &'a str,
) -> ApplyRatingArgs<'a> {
    ApplyRatingArgs {
        chunk_id: &r.chunk_id,
        usefulness: r.usefulness,
        harmful: r.harmful,
        justification: &r.justification,
        caller_stream: &auth.stream_id,
        caller_is_admin: auth.is_admin,
        agent_id: &auth.stream_id,
        model_version: &payload.model_version,
        prompt_version: &payload.prompt_version,
        trajectory_id: payload.trajectory_id.as_deref(),
        now_unix_ms: now_ms,
        event_id,
    }
}
