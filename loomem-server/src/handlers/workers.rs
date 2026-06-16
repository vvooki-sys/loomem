use axum::{
    extract::{Path, Query, State},
    Json,
};
use chrono::Utc;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::types::{
    StreamStats, StreamStatsParams, StreamStatsResponse, WorkerControlResponse, WorkerInfo,
    WorkersPauseResponse, WorkersStatusResponse,
};
use super::AppError;
use crate::AppState;

use super::admin::require_admin;
use loomem_core::workers_registry::KNOWN_WORKERS;

// ── Background worker control (for eval runs) ─────────────────────────────────

pub async fn admin_workers_pause_handler(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Result<Json<WorkersPauseResponse>, AppError> {
    require_admin(&request)?;

    for name in KNOWN_WORKERS {
        if let Some(w) = state.workers.get(*name) {
            w.paused.store(true, Ordering::SeqCst);
        }
    }

    Ok(Json(WorkersPauseResponse {
        paused: true,
        message: "All workers paused".to_string(),
    }))
}

pub async fn admin_workers_resume_handler(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Result<Json<WorkersPauseResponse>, AppError> {
    require_admin(&request)?;

    for name in KNOWN_WORKERS {
        if let Some(w) = state.workers.get(*name) {
            w.paused.store(false, Ordering::SeqCst);
        }
    }

    Ok(Json(WorkersPauseResponse {
        paused: false,
        message: "All workers resumed".to_string(),
    }))
}

pub async fn admin_workers_status_handler(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Result<Json<WorkersStatusResponse>, AppError> {
    require_admin(&request)?;

    let mut worker_infos: Vec<WorkerInfo> = Vec::with_capacity(KNOWN_WORKERS.len());
    let mut all_paused = true;

    for name in KNOWN_WORKERS {
        if let Some(w) = state.workers.get(*name) {
            let paused = w.paused.load(Ordering::SeqCst);
            if !paused {
                all_paused = false;
            }
            worker_infos.push(WorkerInfo {
                name: (*name).to_string(),
                paused,
                last_run_at: w.last_run_at.load(Ordering::SeqCst),
                last_success_at: w.last_success_at.load(Ordering::SeqCst),
                items_processed_total: w.items_processed_total.load(Ordering::SeqCst),
                interval_secs: w.interval_secs,
            });
        }
    }

    // If no workers are registered, the registry is empty — treat as not paused.
    if worker_infos.is_empty() {
        all_paused = false;
    }

    Ok(Json(WorkersStatusResponse {
        paused: all_paused,
        workers: worker_infos,
    }))
}

pub async fn admin_workers_pause_one_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Result<Json<WorkerControlResponse>, AppError> {
    require_admin(&request)?;

    if !KNOWN_WORKERS.contains(&name.as_str()) {
        return Err(AppError::NotFound(format!("unknown worker: {name}")));
    }

    if let Some(w) = state.workers.get(name.as_str()) {
        w.paused.store(true, Ordering::SeqCst);
    }

    Ok(Json(WorkerControlResponse {
        name: name.clone(),
        paused: true,
        message: format!("Worker {name} paused"),
    }))
}

pub async fn admin_workers_resume_one_handler(
    Path(name): Path<String>,
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Result<Json<WorkerControlResponse>, AppError> {
    require_admin(&request)?;

    if !KNOWN_WORKERS.contains(&name.as_str()) {
        return Err(AppError::NotFound(format!("unknown worker: {name}")));
    }

    if let Some(w) = state.workers.get(name.as_str()) {
        w.paused.store(false, Ordering::SeqCst);
    }

    Ok(Json(WorkerControlResponse {
        name: name.clone(),
        paused: false,
        message: format!("Worker {name} resumed"),
    }))
}

/// GET /admin/streams/stats — per-stream aggregated stats for admin UI.
/// Optional `?stream=<id>` filters to a single stream.
pub async fn admin_streams_stats_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<StreamStatsParams>,
    request: axum::extract::Request,
) -> Result<Json<StreamStatsResponse>, AppError> {
    require_admin(&request)?;

    let filter_stream = params.stream.as_deref();

    let mut streams: std::collections::HashMap<String, StreamStats> =
        std::collections::HashMap::new();
    // (first_ts, last_ts, last_access_ts) as raw epoch seconds per stream.
    let mut activity_epochs: std::collections::HashMap<String, (u64, u64, u64)> =
        std::collections::HashMap::new();

    // Pass 1: scan chunks (including soft-deleted) and aggregate.
    for prefix in &[b"chunk:L0:", b"chunk:L1:"] {
        for (_, value) in state.store.prefix_scan(*prefix) {
            let chunk: loomem_core::storage::Chunk = match serde_json::from_slice(&value) {
                Ok(c) => c,
                Err(_) => continue,
            };

            if let Some(s) = filter_stream {
                if chunk.stream != s {
                    continue;
                }
            }

            let entry = streams.entry(chunk.stream.clone()).or_default();

            if chunk.deleted_at.is_some() {
                entry.chunks.soft_deleted += 1;
                continue;
            }

            entry.chunks.total += 1;

            let level_key = format!("L{}", chunk.level);
            *entry.chunks.by_level.entry(level_key).or_insert(0) += 1;

            let type_key = chunk
                .memory_type
                .clone()
                .unwrap_or_else(|| "UNKNOWN".to_string());
            *entry.chunks.by_type.entry(type_key).or_insert(0) += 1;

            if chunk.dormant {
                entry.chunks.dormant += 1;
            }

            if chunk.level == 0 && !chunk.consolidated {
                entry.consolidation.pending_l0 += 1;
            }
            if chunk.level == 1 {
                entry.consolidation.consolidated_l1 += 1;
            }
            if !chunk.is_latest || chunk.supersedes_id.is_some() {
                entry.consolidation.contradiction_count += 1;
            }

            entry.storage.estimated_bytes += chunk.content.len() as u64;

            let act = activity_epochs
                .entry(chunk.stream.clone())
                .or_insert((u64::MAX, 0, 0));
            let ts = chunk.timestamp;
            if ts > 0 {
                if ts < act.0 {
                    act.0 = ts;
                }
                if ts > act.1 {
                    act.1 = ts;
                }
            }
            let access_ts = chunk.last_implicit_boost.or(chunk.updated_at).unwrap_or(0);
            if access_ts > act.2 {
                act.2 = access_ts;
            }
        }
    }

    // Pass 2: count graph entities per stream.
    for (_, value) in state.store.prefix_scan(b"graph:entity:") {
        if let Ok(entity) = serde_json::from_slice::<loomem_core::graph::EntityNode>(&value) {
            if let Some(s) = filter_stream {
                if entity.stream_id != s {
                    continue;
                }
            }
            streams
                .entry(entity.stream_id.clone())
                .or_default()
                .graph
                .entities += 1;
        }
    }

    // Pass 3: count graph edges per stream.
    for (_, value) in state.store.prefix_scan(b"graph:edge:") {
        if let Ok(edge) = serde_json::from_slice::<loomem_core::graph::Edge>(&value) {
            if let Some(s) = filter_stream {
                if edge.stream_id != s {
                    continue;
                }
            }
            streams
                .entry(edge.stream_id.clone())
                .or_default()
                .graph
                .edges += 1;
        }
    }

    // Format activity timestamps once per stream.
    for (stream_id, (first, last, access)) in activity_epochs {
        if let Some(entry) = streams.get_mut(&stream_id) {
            if first != u64::MAX {
                entry.activity.first_chunk_at = Some(epoch_to_rfc3339(first));
            }
            if last > 0 {
                entry.activity.last_chunk_at = Some(epoch_to_rfc3339(last));
            }
            if access > 0 {
                entry.activity.last_access_at = Some(epoch_to_rfc3339(access));
            }
        }
    }

    let total_streams = streams.len();
    Ok(Json(StreamStatsResponse {
        streams,
        total_streams,
        generated_at: Utc::now().to_rfc3339(),
    }))
}

fn epoch_to_rfc3339(secs: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs as i64, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| String::from("1970-01-01T00:00:00+00:00"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use loomem_core::workers_registry::{build_registry, KNOWN_WORKERS};

    fn make_test_registry() -> loomem_core::workers_registry::WorkerRegistry {
        let cfg = loomem_core::scheduler::WorkerConfig::default();
        build_registry(&cfg, 86400, 3600)
    }

    #[test]
    fn build_registry_has_correct_worker_count() {
        let reg = make_test_registry();
        assert_eq!(reg.len(), KNOWN_WORKERS.len());
        assert_eq!(reg.len(), 7);
    }

    #[test]
    fn all_workers_default_not_paused() {
        let reg = make_test_registry();
        for name in KNOWN_WORKERS {
            let w = reg.get(*name).expect("worker missing");
            assert!(
                !w.paused.load(Ordering::SeqCst),
                "worker {name} should not be paused by default"
            );
        }
    }

    #[test]
    fn pause_one_does_not_affect_others() {
        let reg = make_test_registry();
        reg.get("consolidation")
            .unwrap()
            .paused
            .store(true, Ordering::SeqCst);

        assert!(reg
            .get("consolidation")
            .unwrap()
            .paused
            .load(Ordering::SeqCst));
        assert!(!reg.get("decay").unwrap().paused.load(Ordering::SeqCst));
        assert!(!reg.get("compaction").unwrap().paused.load(Ordering::SeqCst));
    }

    #[test]
    fn global_pause_sets_all_workers() {
        let reg = make_test_registry();
        for name in KNOWN_WORKERS {
            reg.get(*name).unwrap().paused.store(true, Ordering::SeqCst);
        }
        for name in KNOWN_WORKERS {
            assert!(
                reg.get(*name).unwrap().paused.load(Ordering::SeqCst),
                "worker {name} should be paused"
            );
        }
    }

    #[test]
    fn global_resume_clears_all_workers() {
        let reg = make_test_registry();
        // First pause all
        for name in KNOWN_WORKERS {
            reg.get(*name).unwrap().paused.store(true, Ordering::SeqCst);
        }
        // Then resume all
        for name in KNOWN_WORKERS {
            reg.get(*name)
                .unwrap()
                .paused
                .store(false, Ordering::SeqCst);
        }
        for name in KNOWN_WORKERS {
            assert!(
                !reg.get(*name).unwrap().paused.load(Ordering::SeqCst),
                "worker {name} should not be paused after resume"
            );
        }
    }

    #[test]
    fn unknown_worker_not_in_known_workers() {
        assert!(!KNOWN_WORKERS.contains(&"unknown_xyz"));
        assert!(!KNOWN_WORKERS.contains(&"entity_extraction"));
        assert!(!KNOWN_WORKERS.contains(&"auto_dream"));
    }

    #[test]
    fn known_workers_contains_expected_names() {
        for name in &[
            "consolidation",
            "decay",
            "compaction",
            "backup",
            "clustering",
            "purge",
            "stats",
        ] {
            assert!(
                KNOWN_WORKERS.contains(name),
                "KNOWN_WORKERS missing: {name}"
            );
        }
    }
}
