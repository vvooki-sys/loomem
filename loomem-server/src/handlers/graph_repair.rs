//! POST /v1/admin/repair/graph-entity-streams handler.
//!
//! Repairs `graph:entity:*` rows that have an empty `stream_id` (legacy cohort
//! written before multi-stream support). Resolves the stream from paired chunks
//! and re-writes through the encrypted chokepoint.
//!
//! Handler check order (mirrors ADR-013 §7 pattern):
//!   admin(403) → noop(400) → busy(409) → 200 with repair report.
//!
//! Handler is SYNCHRONOUS: ~1 k entities × a few point-gets = seconds.
//! Precedent: `rekey_name_index_handler`.
//!
//! Lesson /151: operator tooling defaults to `dry_run = true`.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use axum::{extract::State, http::StatusCode, Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::AuthContext;
use crate::handlers::AppError;
use crate::AppState;

// ── Running guard ─────────────────────────────────────────────────────────────

static RUNNING: AtomicBool = AtomicBool::new(false);

/// RAII guard: clears `RUNNING` on drop (panic-safe).
struct RunningGuard;

impl Drop for RunningGuard {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

// ── Request type ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct GraphRepairRequest {
    #[serde(default = "default_dry_run")]
    pub dry_run: bool,
}

/// Single source of truth for the default: operator tooling defaults to
/// preview (lesson /151). Used by BOTH the serde default (body `{}`) and the
/// no-body fallback in the handler.
const DEFAULT_DRY_RUN: bool = true;

fn default_dry_run() -> bool {
    DEFAULT_DRY_RUN
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// POST /v1/admin/repair/graph-entity-streams
pub async fn graph_entity_stream_repair_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    body: Option<Json<GraphRepairRequest>>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    // 1. Admin gate (403).
    if !auth.is_admin {
        return Err(AppError::Forbidden("admin access required".into()));
    }

    // 2. NoopProvider check (400).
    if !state.store.encryption_provider().is_enabled() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "encryption provider is disabled (NoopProvider); \
                          set LOOMEM_AT_REST_MASTER_KEY before running repair"
            })),
        ));
    }

    // 3. Busy check (409).
    if RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({ "status": "already_running" })),
        ));
    }

    let _guard = RunningGuard;

    let dry_run = body.map(|Json(r)| r.dry_run).unwrap_or(DEFAULT_DRY_RUN);

    // 4. Run repair (synchronous — expected to complete in seconds).
    let report =
        loomem_core::graph_repair::repair_entity_streams(&state.store, &state.graph, dry_run)
            .map_err(AppError::Internal)?;

    Ok((
        StatusCode::OK,
        Json(
            serde_json::to_value(&report)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("serialize repair report: {e}")))?,
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Critic /147a MED-3: the dry_run default is contract, test it directly.
    /// Empty JSON body `{}` → serde default kicks in → dry_run == true, and it
    /// equals the shared constant used by the no-body fallback.
    #[test]
    fn dry_run_defaults_true_for_empty_body() {
        let req: GraphRepairRequest = serde_json::from_str("{}").expect("parse {}");
        assert!(req.dry_run, "empty body must default to dry_run=true");
        assert_eq!(
            req.dry_run, DEFAULT_DRY_RUN,
            "serde default == no-body fallback"
        );
    }

    /// Explicit `false` must be honored (the apply path is reachable).
    #[test]
    fn dry_run_explicit_false_is_honored() {
        let req: GraphRepairRequest =
            serde_json::from_str(r#"{"dry_run": false}"#).expect("parse explicit");
        assert!(!req.dry_run);
    }
}
