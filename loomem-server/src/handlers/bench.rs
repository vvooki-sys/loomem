//! Admin handlers for Reality bench (cycle /128).
//!
//! - `POST /v1/admin/bench/run` — spawn `eval/reality_bench.py --snapshot --json` as a
//!   detached subprocess and return immediately with a task id. Subprocess args are
//!   hard-whitelisted (D8): fixed binary, fixed script path, fixed args, fixed
//!   timeout (5 min), fixed output directory (`eval/`). No request-controlled input
//!   reaches `Command`.
//! - `GET /v1/admin/bench/history` — enumerate `eval/auto-improve-baseline-*.json`
//!   and `eval/reality-bench-*.json`, return metadata array sorted by timestamp asc.

use axum::Json;
use chrono::Utc;
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

use super::AppError;
use crate::auth::AuthContext;

/// Whitelisted subprocess interpreter — resolved via PATH.
const BENCH_INTERPRETER: &str = "python3";

/// Whitelisted script path, relative to the loomem-server CWD (repo root).
const BENCH_SCRIPT_REL: &str = "eval/reality_bench.py";

/// Whitelisted output directory.
const BENCH_OUTPUT_DIR: &str = "eval";

/// Hard subprocess timeout. Matches D8 in the brief.
const BENCH_TIMEOUT_SECS: u64 = 300;

#[derive(Serialize)]
pub struct BenchRunResponse {
    pub task_id: String,
    pub started_at: String,
}

#[derive(Serialize)]
pub struct BenchHistoryEntry {
    pub filename: String,
    pub kind: &'static str,
    pub timestamp: String,
    pub hit_rate: f64,
    pub total_questions: u64,
    pub by_category: Value,
}

/// Admin gate for in-handler defence-in-depth: 403 unless the request carries
/// an admin `AuthContext`. Shared with the `/v1/encryption/status` handler
/// (cycle /144) — exposed `pub(crate)` rather than duplicated (auth logic is
/// not copy-pasted). Covered by the two gate tests below.
pub(crate) fn require_admin_forbidden(req: &axum::extract::Request) -> Result<(), AppError> {
    match req.extensions().get::<AuthContext>() {
        Some(ctx) if ctx.is_admin => Ok(()),
        _ => Err(AppError::Forbidden("admin access required".into())),
    }
}

pub async fn admin_bench_run_handler(
    request: axum::extract::Request,
) -> Result<Json<BenchRunResponse>, AppError> {
    require_admin_forbidden(&request)?;

    let task_id = uuid::Uuid::new_v4().to_string();
    let started_at = Utc::now().to_rfc3339();

    let task_id_for_log = task_id.clone();
    tokio::spawn(async move {
        spawn_snapshot_subprocess(&task_id_for_log).await;
    });

    Ok(Json(BenchRunResponse {
        task_id,
        started_at,
    }))
}

/// Run `python3 eval/reality_bench.py --snapshot --json` with no caller-supplied
/// args and a hard 5-minute wall clock. Logs failures; never panics.
async fn spawn_snapshot_subprocess(task_id: &str) {
    let mut cmd = Command::new(BENCH_INTERPRETER);
    cmd.arg(BENCH_SCRIPT_REL)
        .arg("--snapshot")
        .arg("--json")
        .kill_on_drop(true);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(task_id = %task_id, error = %e, "bench subprocess spawn failed");
            return;
        }
    };

    let mut child = child;
    match timeout(Duration::from_secs(BENCH_TIMEOUT_SECS), child.wait()).await {
        Ok(Ok(status)) => {
            tracing::info!(task_id = %task_id, code = ?status.code(), "bench subprocess exited");
        }
        Ok(Err(e)) => {
            tracing::error!(task_id = %task_id, error = %e, "bench subprocess wait failed");
        }
        Err(_) => {
            tracing::warn!(task_id = %task_id, "bench subprocess timed out after {}s, killing",
                BENCH_TIMEOUT_SECS);
            if let Err(e) = child.kill().await {
                tracing::error!(task_id = %task_id, error = %e, "bench subprocess kill failed");
            }
        }
    }
}

pub async fn admin_bench_history_handler(
    request: axum::extract::Request,
) -> Result<Json<Vec<BenchHistoryEntry>>, AppError> {
    require_admin_forbidden(&request)?;
    // Blocking syscalls (read_dir + read per file) run on the dedicated blocking
    // pool so the async worker thread is not stalled while scanning 15+ snapshot
    // JSON files. Fixed path constant => no caller input crosses the boundary.
    let entries = tokio::task::spawn_blocking(|| collect_history(Path::new(BENCH_OUTPUT_DIR)))
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("bench history scan task failed: {e}")))?;
    Ok(Json(entries))
}

fn collect_history(eval_dir: &Path) -> Vec<BenchHistoryEntry> {
    let mut entries = Vec::new();
    let read = match std::fs::read_dir(eval_dir) {
        Ok(r) => r,
        Err(_) => return entries,
    };

    for dirent in read.flatten() {
        let path = dirent.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let kind = classify_snapshot_filename(name);
        if kind.is_none() {
            continue;
        }
        if let Some(entry) = parse_history_entry(&path, name, kind.unwrap()) {
            entries.push(entry);
        }
    }

    entries.sort_by(|a, b| match a.timestamp.cmp(&b.timestamp) {
        std::cmp::Ordering::Equal => a.filename.cmp(&b.filename),
        other => other,
    });
    entries
}

fn classify_snapshot_filename(name: &str) -> Option<&'static str> {
    if !name.ends_with(".json") {
        return None;
    }
    if name.starts_with("reality-bench-") {
        return Some("reality-bench");
    }
    if name.starts_with("auto-improve-baseline-") {
        return Some("auto-improve");
    }
    None
}

fn parse_history_entry(
    path: &PathBuf,
    name: &str,
    kind: &'static str,
) -> Option<BenchHistoryEntry> {
    let raw = std::fs::read(path).ok()?;
    let v: Value = serde_json::from_slice(&raw).ok()?;
    let hit_rate = v.get("hit_rate")?.as_f64()?;
    let timestamp = v
        .get("timestamp")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let total_questions = v
        .get("total_questions")
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let by_category = v.get("by_category").cloned().unwrap_or(Value::Null);
    Some(BenchHistoryEntry {
        filename: name.to_string(),
        kind,
        timestamp,
        hit_rate,
        total_questions,
        by_category,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_recognizes_reality_bench() {
        assert_eq!(
            classify_snapshot_filename("reality-bench-2026-05-18T120000Z.json"),
            Some("reality-bench")
        );
    }

    #[test]
    fn classify_recognizes_auto_improve() {
        assert_eq!(
            classify_snapshot_filename("auto-improve-baseline-2026-05-01.json"),
            Some("auto-improve")
        );
    }

    #[test]
    fn classify_rejects_unrelated() {
        assert_eq!(classify_snapshot_filename("questions.json"), None);
        assert_eq!(classify_snapshot_filename("reality-bench-2026.txt"), None);
        assert_eq!(classify_snapshot_filename("README.md"), None);
    }

    #[test]
    fn collect_history_sorts_ascending_by_timestamp() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("auto-improve-baseline-2026-05-18.json"),
            r#"{"timestamp": "2026-05-18T00:00:00", "hit_rate": 70.0, "total_questions": 80}"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("auto-improve-baseline-2026-03-23.json"),
            r#"{"timestamp": "2026-03-23T00:00:00", "hit_rate": 90.0, "total_questions": 80}"#,
        )
        .unwrap();
        std::fs::write(tmp.path().join("ignored.json"), "{}").unwrap();

        let entries = collect_history(tmp.path());
        assert_eq!(entries.len(), 2, "should include 2 baseline files");
        assert_eq!(entries[0].timestamp, "2026-03-23T00:00:00");
        assert_eq!(entries[1].timestamp, "2026-05-18T00:00:00");
        assert_eq!(entries[0].hit_rate, 90.0);
    }

    #[test]
    fn parse_history_entry_round_trips_by_category() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let with_cat = tmp.path().join("reality-bench-2026-05-18T120000Z.json");
        std::fs::write(
            &with_cat,
            r#"{"timestamp":"2026-05-18T12:00:00","hit_rate":80.0,"total_questions":50,"by_category":{"general":{"hits":3,"total":5}}}"#,
        )
        .unwrap();
        let without_cat = tmp.path().join("auto-improve-baseline-2026-05-01.json");
        std::fs::write(
            &without_cat,
            r#"{"timestamp":"2026-05-01T00:00:00","hit_rate":70.0,"total_questions":80}"#,
        )
        .unwrap();

        let with_entry = parse_history_entry(
            &with_cat,
            "reality-bench-2026-05-18T120000Z.json",
            "reality-bench",
        )
        .expect("entry parses");
        let by_cat = with_entry
            .by_category
            .get("general")
            .expect("by_category.general present");
        assert_eq!(by_cat.get("hits").and_then(Value::as_u64), Some(3));
        assert_eq!(by_cat.get("total").and_then(Value::as_u64), Some(5));

        let without_entry = parse_history_entry(
            &without_cat,
            "auto-improve-baseline-2026-05-01.json",
            "auto-improve",
        )
        .expect("entry parses");
        assert!(
            without_entry.by_category.is_null(),
            "legacy snapshots without by_category default to Value::Null"
        );
    }

    /// AC7: non-admin returns Forbidden (HTTP 403 via AppError::Forbidden mapping).
    #[test]
    fn require_admin_forbidden_rejects_non_admin() {
        use crate::auth::{AuthContext, KeyScope};
        use loomem_core::storage::UserRole;

        let mut req = axum::extract::Request::new(axum::body::Body::empty());
        let ctx =
            AuthContext::single_stream("alice", UserRole::Writer, KeyScope::Private, None, false);
        req.extensions_mut().insert(ctx);

        let err = require_admin_forbidden(&req).expect_err("non-admin must be rejected");
        match err {
            AppError::Forbidden(_) => {}
            other => panic!("expected Forbidden, got {:?}", other),
        }
    }

    /// AC7: admin AuthContext passes the gate.
    #[test]
    fn require_admin_forbidden_admits_admin() {
        use crate::auth::{AuthContext, KeyScope};
        use loomem_core::storage::UserRole;

        let mut req = axum::extract::Request::new(axum::body::Body::empty());
        let ctx = AuthContext::single_stream("root", UserRole::Admin, KeyScope::Shared, None, true);
        req.extensions_mut().insert(ctx);

        require_admin_forbidden(&req).expect("admin must pass");
    }
}
