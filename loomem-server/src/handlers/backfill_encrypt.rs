//! POST /v1/admin/backfill/encrypt-at-rest and GET …/status handlers.
//!
//! Admin-gated (non-admin → 403). POST validates snapshot_token, checks
//! provider is enabled, and spawns a background task. Returns 409 when a run
//! is already active. GET returns the persisted progress or `{"status":"never_run"}`.
//!
//! Handler check order (contract ADR-013 §7):
//!   admin(403) → token(400) → noop(400) → busy(409) → 200 started.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use axum::{extract::State, http::StatusCode, Extension, Json};
use chrono::{NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use loomem_core::backfill_trace::TraceLog;
use loomem_core::encrypt_backfill::{BackfillProgress, EncryptBackfillParams};
use loomem_core::storage::keys::ENCRYPT_BACKFILL_PROGRESS_KEY;

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

// ── Request / response types ──────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct BackfillEncryptRequest {
    pub snapshot_token: String,
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    #[serde(default)]
    pub inter_batch_sleep_ms: u64,
}

fn default_batch_size() -> usize {
    200
}

// ── Snapshot-token validation (pure, clock-injected for testability) ──────────

/// Validate `token` against the regex `^snap-(\d{8})-([a-z0-9-]+)-([0-9a-f]+)$`
/// and check that the embedded YYYYMMDD date is within the last 7 days
/// (inclusive) relative to `today`.
///
/// Returns `Ok(())` on success, `Err(String)` with a human-readable message
/// on failure.
pub fn validate_snapshot_token(token: &str, today: NaiveDate) -> Result<(), String> {
    let date_part = parse_token_structure(token)?;

    // Date staleness check: parse YYYYMMDD and compare to today.
    let token_date = NaiveDate::parse_from_str(date_part, "%Y%m%d")
        .map_err(|e| format!("snapshot_token date parse error: {e}"))?;

    let age_days = (today - token_date).num_days();
    if !(0..=7).contains(&age_days) {
        return Err(format!(
            "snapshot_token date {date_part} is {age_days} days from today \
             (must be 0–7 days in the past)"
        ));
    }

    Ok(())
}

/// Structural check via manual parse (no regex dep). Expected shape per
/// ADR-013 §7: `snap-{YYYYMMDD}-{instance}-{hex_id}` matching
/// `^snap-(\d{8})-([a-z0-9-]+)-([0-9a-f]+)$` (lowercase only). Returns the
/// date segment on success.
fn parse_token_structure(token: &str) -> Result<&str, String> {
    let rest = token
        .strip_prefix("snap-")
        .ok_or_else(|| format!("snapshot_token must start with 'snap-': {token}"))?;

    // Split on the first '-' to get the date segment.
    let (date_part, after_date) = rest
        .split_once('-')
        .ok_or_else(|| format!("snapshot_token missing date segment: {token}"))?;

    if date_part.len() != 8 || !date_part.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!(
            "snapshot_token date segment must be 8 digits (YYYYMMDD): {token}"
        ));
    }

    // Split remaining on the last '-' to isolate the hex suffix.
    let (instance, hex_id) = after_date
        .rsplit_once('-')
        .ok_or_else(|| format!("snapshot_token missing hex suffix: {token}"))?;

    check_instance_and_hex(token, instance, hex_id)?;
    Ok(date_part)
}

/// Charset checks per the ADR regex: instance `[a-z0-9-]+`, hex `[0-9a-f]+`.
fn check_instance_and_hex(token: &str, instance: &str, hex_id: &str) -> Result<(), String> {
    if instance.is_empty()
        || !instance
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "snapshot_token instance must be non-empty [a-z0-9-]: {token}"
        ));
    }
    if hex_id.is_empty()
        || !hex_id
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    {
        return Err(format!(
            "snapshot_token hex_id must be non-empty lowercase hex: {token}"
        ));
    }
    Ok(())
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// POST /v1/admin/backfill/encrypt-at-rest
pub async fn encrypt_at_rest_backfill_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(req): Json<BackfillEncryptRequest>,
) -> Result<(StatusCode, Json<Value>), AppError> {
    // 1. Admin gate (403).
    if !auth.is_admin {
        return Err(AppError::Forbidden("admin access required".into()));
    }

    // 2. Token validation (400).
    let today = Utc::now().date_naive();
    if let Err(msg) = validate_snapshot_token(&req.snapshot_token, today) {
        return Ok((StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))));
    }

    // 3. NoopProvider check (400).
    if !state.store.encryption_provider().is_enabled() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "encryption provider is disabled (NoopProvider); \
                          set LOOMEM_AT_REST_MASTER_KEY before running backfill"
            })),
        ));
    }

    // 4. Busy check (409).
    if RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({ "status": "already_running" })),
        ));
    }

    // Guard drops RUNNING on exit (panic-safe).
    let _guard = RunningGuard;
    let store = state.store.clone();
    let graph = state.graph.clone();
    let data_dir = state.config.storage.data_dir.clone();
    let snapshot_token = req.snapshot_token.clone();
    let batch_size = req.batch_size;
    let inter_batch_sleep_ms = req.inter_batch_sleep_ms;

    // 5. Spawn background task (guard moved into task).
    tokio::spawn(async move {
        // Guard keeps RUNNING=true until the task exits.
        let _guard = _guard;
        // data_dir is sourced from config.toml (UTF-8); non-UTF8 paths are
        // impossible on production (Linux/macOS). Fallback "." keeps the trace
        // writable rather than panicking — never logs to the wrong place.
        let data_dir_str = data_dir.to_str().unwrap_or(".");
        let trace = TraceLog::new(data_dir_str);
        let params = EncryptBackfillParams {
            snapshot_token,
            batch_size,
            inter_batch_sleep_ms,
        };
        match loomem_core::encrypt_backfill::run_encrypt_backfill(&store, &graph, &params, &trace)
            .await
        {
            Ok(prog) => {
                tracing::info!(status = %prog.status, "encrypt-at-rest backfill finished");
            }
            Err(e) => {
                tracing::error!(error = %e, "encrypt-at-rest backfill failed");
                // Write a failed progress record so GET /status reflects the error.
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let fail_prog = BackfillProgress {
                    status: "failed".to_string(),
                    snapshot_token: params.snapshot_token.clone(),
                    started_at: now,
                    updated_at: now,
                    per_class: Default::default(),
                    error: Some(format!("{e:#}")),
                };
                if let Ok(bytes) = serde_json::to_vec(&fail_prog) {
                    let _ = store.put(ENCRYPT_BACKFILL_PROGRESS_KEY, &bytes);
                }
                trace.emit(
                    "encrypt_backfill_error",
                    serde_json::json!({ "error": format!("{e:#}") }),
                );
            }
        }
    });

    Ok((
        StatusCode::OK,
        Json(json!({
            "status": "started",
            "snapshot_token": req.snapshot_token,
            "batch_size": req.batch_size,
        })),
    ))
}

/// GET /v1/admin/backfill/encrypt-at-rest/status
pub async fn encrypt_at_rest_backfill_status_handler(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<Value>, AppError> {
    if !auth.is_admin {
        return Err(AppError::Forbidden("admin access required".into()));
    }

    match state.store.get(ENCRYPT_BACKFILL_PROGRESS_KEY)? {
        None => Ok(Json(json!({ "status": "never_run" }))),
        Some(bytes) => {
            let prog: BackfillProgress = serde_json::from_slice(&bytes)
                .context("failed to deserialize backfill progress")?;
            Ok(Json(
                serde_json::to_value(&prog).context("serialize progress")?,
            ))
        }
    }
}

use anyhow::Context as _;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    // ── T6 — validate_snapshot_token unit tests ───────────────────────────────

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 6, 10).expect("valid date")
    }

    fn yesterday() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 6, 9).expect("valid date")
    }

    fn eight_days_ago() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 6, 2).expect("valid date")
    }

    fn seven_days_ago() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 6, 3).expect("valid date")
    }

    #[test]
    fn t6_valid_token_today() {
        assert!(validate_snapshot_token("snap-20260610-prod-aabb1234", today()).is_ok());
    }

    #[test]
    fn t6_valid_token_yesterday() {
        assert!(validate_snapshot_token("snap-20260609-prod-aabb1234", today()).is_ok());
    }

    #[test]
    fn t6_valid_token_seven_days_ago() {
        // Boundary: exactly 7 days is still valid.
        let _ = seven_days_ago(); // suppress warning
        assert!(validate_snapshot_token("snap-20260603-prod-aabb1234", today()).is_ok());
    }

    #[test]
    fn t6_token_eight_days_ago_fails() {
        let _ = eight_days_ago();
        let err = validate_snapshot_token("snap-20260602-prod-aabb1234", today())
            .expect_err("must fail for 8-day-old token");
        assert!(
            err.contains("days from today") || err.contains("must be 0"),
            "{err}"
        );
    }

    #[test]
    fn t6_malformed_no_snap_prefix() {
        let err =
            validate_snapshot_token("tok-20260610-prod-aabb", today()).expect_err("must fail");
        assert!(err.contains("snap-"), "{err}");
    }

    #[test]
    fn t6_malformed_bad_date() {
        let err =
            validate_snapshot_token("snap-2026061X-prod-aabb", today()).expect_err("must fail");
        assert!(err.contains("8 digits") || err.contains("date"), "{err}");
    }

    #[test]
    fn t6_malformed_no_hex_suffix() {
        // All-letter suffix is not hex.
        let err =
            validate_snapshot_token("snap-20260610-prod-ZZZZ", today()).expect_err("must fail");
        assert!(err.contains("hex"), "{err}");
    }

    #[test]
    fn t6_malformed_no_instance() {
        // Missing instance segment (only one '-' between date and hex).
        let err = validate_snapshot_token("snap-20260610-aabb", today()).expect_err("must fail");
        // With only one dash after the date, rsplit_once('-') on "aabb" returns None.
        assert!(!err.is_empty(), "{err}");
    }

    #[test]
    fn t6_future_date_fails() {
        // Date in the future is also rejected (age_days < 0).
        let err = validate_snapshot_token("snap-20260615-prod-aabb1234", today())
            .expect_err("future token must fail");
        assert!(err.contains("days from today"), "{err}");
    }

    // ── T6 (yesterday helper) ─────────────────────────────────────────────────

    #[test]
    fn t6_yesterday_is_valid() {
        let _ = yesterday();
        assert!(validate_snapshot_token("snap-20260609-myinst-deadbeef", today()).is_ok());
    }

    // ── T6 — lowercase-only charset (ADR regex [a-z0-9-] / [0-9a-f]) ──────────

    #[test]
    fn t6_uppercase_instance_fails() {
        let err = validate_snapshot_token("snap-20260610-PROD-aabb1234", today())
            .expect_err("uppercase instance must fail");
        assert!(err.contains("instance"), "{err}");
    }

    #[test]
    fn t6_uppercase_hex_fails() {
        let err = validate_snapshot_token("snap-20260610-prod-AABB1234", today())
            .expect_err("uppercase hex must fail");
        assert!(err.contains("hex"), "{err}");
    }
}
