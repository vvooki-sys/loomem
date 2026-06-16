//! `GET /v1/co_occur` — Temporal co-occurrence POC endpoint (cycle/107).
//!
//! PAM-inspired (arXiv 2602.11322) read-only endpoint for manual eval.
//! Returns chunks from the same session_id OR within ±N hours of a reference
//! chunk's timestamp. ZERO integration with the retrieval pipeline (RRF, search,
//! ranking). Kill/scale decision lives in cycle/108 gate post manual eval.

use std::sync::Arc;

use anyhow::Context as _;
use axum::extract::{Query, State};
use axum::Json;
use chrono::SecondsFormat;

use super::AppError;
use crate::auth;
use crate::AppState;

// ── Query / Response types ────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
pub struct CoOccurQuery {
    pub chunk_id: String,
    #[serde(default = "default_window_hours")]
    pub window_hours: u32,
    #[serde(default = "default_top_k")]
    pub top_k: u32,
}

fn default_window_hours() -> u32 {
    48
}
fn default_top_k() -> u32 {
    20
}

#[derive(serde::Serialize)]
pub struct CoOccurResponse {
    pub reference_chunk_id: String,
    pub reference_timestamp: String,
    pub reference_session_id: Option<String>,
    pub results: Vec<CoOccurResult>,
    pub total_in_window: usize,
    pub returned: usize,
}

#[derive(serde::Serialize)]
pub struct CoOccurResult {
    pub chunk_id: String,
    pub timestamp: String,
    pub session_id: Option<String>,
    pub delta_seconds: i64,
    pub same_session: bool,
    pub text_preview: String,
    pub stream_id: String,
}

// ── Handler ───────────────────────────────────────────────────────────────────

/// `GET /v1/co_occur` — temporal co-occurrence for a reference chunk.
pub async fn co_occur_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<crate::auth::AuthContext>,
    Query(params): Query<CoOccurQuery>,
) -> Result<Json<CoOccurResponse>, AppError> {
    // 1. Resolve caller stream (no cross-stream param for this POC).
    let stream = auth::validate_stream(&auth, None)
        .map_err(|_| AppError::Forbidden("Access denied".into()))?;

    // 2. Validate params.
    if params.window_hours > 168 {
        return Err(AppError::BadRequest("window_hours must be ≤ 168".into()));
    }
    if params.top_k > 50 {
        return Err(AppError::BadRequest("top_k must be ≤ 50".into()));
    }

    // 3. Load reference chunk (None = 404 per brief; treats wrong-stream as 404,
    //    fail-closed consistent with handlers/delete.rs pattern).
    let ref_chunk = state
        .store
        .get_chunk_scoped(&params.chunk_id, &stream)
        .context("store.get_chunk_scoped failed")?
        .ok_or_else(|| AppError::NotFound(format!("chunk '{}' not found", params.chunk_id)))?;

    // Treat soft-deleted reference chunk as not found (fail-closed).
    if ref_chunk.deleted_at.is_some() {
        return Err(AppError::NotFound(format!(
            "chunk '{}' not found",
            params.chunk_id
        )));
    }

    // 4. Extract reference session_id from metadata.
    let ref_session = extract_session_id(ref_chunk.metadata.as_ref());

    // 5. Compute window bounds.
    let window_secs = i64::from(params.window_hours) * 3600;
    // i64::try_from: timestamps are Unix epoch u64; all plausible values fit in i64.
    let ref_ts = i64::try_from(ref_chunk.timestamp).unwrap_or(i64::MAX);

    // 6. Load all chunks and filter.
    let all_chunks = state
        .store
        .get_all_chunks()
        .context("store.get_all_chunks failed")?;

    let mut candidates: Vec<CoOccurResult> = all_chunks
        .into_iter()
        .filter_map(|c| {
            if c.stream != stream || c.id == ref_chunk.id || c.dormant {
                return None;
            }
            // i64::try_from: same rationale as ref_ts above.
            let c_ts = i64::try_from(c.timestamp).unwrap_or(i64::MAX);
            let delta = c_ts - ref_ts;
            let c_session = extract_session_id(c.metadata.as_ref());
            let same_session = matches!(
                (&ref_session, &c_session),
                (Some(r), Some(c)) if r == c
            );
            let in_window = delta.abs() <= window_secs;
            if !in_window && !same_session {
                return None;
            }
            Some(CoOccurResult {
                chunk_id: c.id,
                timestamp: epoch_to_rfc3339(c.timestamp),
                session_id: c_session,
                delta_seconds: delta,
                same_session,
                text_preview: c.content.chars().take(200).collect(),
                stream_id: c.stream,
            })
        })
        .collect();

    // 7. Sort: same_session DESC, |delta| ASC.
    candidates.sort_by_key(|r| (!r.same_session, r.delta_seconds.unsigned_abs()));

    let total_in_window = candidates.len();

    // 8. Truncate. u32 → usize: always valid on 32- and 64-bit platforms (u32 ≤ usize::MAX).
    let top_k = usize::try_from(params.top_k).unwrap_or(usize::MAX);
    candidates.truncate(top_k);
    let returned = candidates.len();

    Ok(Json(CoOccurResponse {
        reference_chunk_id: ref_chunk.id,
        reference_timestamp: epoch_to_rfc3339(ref_chunk.timestamp),
        reference_session_id: ref_session,
        results: candidates,
        total_in_window,
        returned,
    }))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Extract `session_id` from `chunk.metadata["session_id"]`.
fn extract_session_id(metadata: Option<&serde_json::Value>) -> Option<String> {
    metadata
        .and_then(|m| m.get("session_id"))
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Convert a Unix epoch (u64) to RFC3339 UTC string.
/// Falls back to the epoch string on overflow (essentially impossible for
/// any timestamp in the foreseeable future, but we must handle None gracefully).
fn epoch_to_rfc3339(secs: u64) -> String {
    // i64 cast: u64 fits in i64 for any timestamp in 0..=i64::MAX range.
    // The conversion is safe for all plausible timestamps (year < 292278994).
    let secs_i64 = i64::try_from(secs).unwrap_or(i64::MAX); // truncation: year >292M = use MAX
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs_i64, 0)
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Unit: sort comparator ────────────────────────────────────────────

    /// Build a minimal CoOccurResult for sort testing.
    fn make_result(chunk_id: &str, delta_seconds: i64, same_session: bool) -> CoOccurResult {
        CoOccurResult {
            chunk_id: chunk_id.to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            session_id: None,
            delta_seconds,
            same_session,
            text_preview: String::new(),
            stream_id: "stream".to_string(),
        }
    }

    #[test]
    fn sort_same_session_wins_over_closer_nonsession() {
        // same_session=true at |delta|=300 must beat same_session=false at |delta|=10.
        let mut results = [make_result("a", 10, false), make_result("b", 300, true)];
        results.sort_by_key(|r| (!r.same_session, r.delta_seconds.unsigned_abs()));
        assert_eq!(
            results[0].chunk_id, "b",
            "same_session=true must sort first"
        );
        assert_eq!(results[1].chunk_id, "a");
    }

    #[test]
    fn sort_within_same_session_smaller_delta_wins() {
        let mut results = [
            make_result("far", 7200, true),
            make_result("near", 60, true),
        ];
        results.sort_by_key(|r| (!r.same_session, r.delta_seconds.unsigned_abs()));
        assert_eq!(results[0].chunk_id, "near");
        assert_eq!(results[1].chunk_id, "far");
    }

    #[test]
    fn sort_within_nonsession_smaller_abs_delta_wins_negative_delta() {
        // Negative delta (candidate is BEFORE ref) — |delta| comparison.
        let mut results = [
            make_result("before_far", -3600, false),
            make_result("after_near", 600, false),
        ];
        results.sort_by_key(|r| (!r.same_session, r.delta_seconds.unsigned_abs()));
        assert_eq!(results[0].chunk_id, "after_near");
        assert_eq!(results[1].chunk_id, "before_far");
    }

    // ── Integration: handler via make_test_app ────────────────────────

    #[tokio::test]
    async fn co_occur_returns_404_for_unknown_chunk() {
        use crate::auth::{AuthContext, KeyScope, UserRole};
        let (_app, state) = crate::tests::make_test_app();
        let auth = AuthContext::single_stream(
            "__user_test__",
            UserRole::Admin,
            KeyScope::Private,
            Some("test_user".to_string()),
            false,
        );
        let params = CoOccurQuery {
            chunk_id: "nonexistent_id".to_string(),
            window_hours: 48,
            top_k: 20,
        };
        let result = co_occur_handler(State(state), axum::Extension(auth), Query(params)).await;
        assert!(
            matches!(result, Err(AppError::NotFound(_))),
            "unknown chunk_id must return 404"
        );
    }

    #[tokio::test]
    async fn co_occur_rejects_out_of_range_params() {
        use crate::auth::{AuthContext, KeyScope, UserRole};
        let (_app, state) = crate::tests::make_test_app();
        let auth = AuthContext::single_stream(
            "__user_test__",
            UserRole::Admin,
            KeyScope::Private,
            Some("test_user".to_string()),
            false,
        );

        // window_hours > 168
        let params = CoOccurQuery {
            chunk_id: "any".to_string(),
            window_hours: 200,
            top_k: 10,
        };
        let result = co_occur_handler(
            State(state.clone()),
            axum::Extension(auth.clone()),
            Query(params),
        )
        .await;
        assert!(matches!(result, Err(AppError::BadRequest(_))));

        // top_k > 50
        let params = CoOccurQuery {
            chunk_id: "any".to_string(),
            window_hours: 48,
            top_k: 51,
        };
        let result = co_occur_handler(State(state), axum::Extension(auth), Query(params)).await;
        assert!(matches!(result, Err(AppError::BadRequest(_))));
    }

    /// Build a minimal test Chunk with sane defaults. Only the fields
    /// meaningful to the co_occur handler need to be overridden in callers.
    fn make_chunk(
        id: &str,
        stream: &str,
        timestamp: u64,
        session_id: Option<&str>,
    ) -> loomem_core::storage::Chunk {
        loomem_core::storage::Chunk {
            id: id.to_string(),
            content: format!("content for {id}"),
            stream: stream.to_string(),
            level: 0,
            score: 1.0,
            timestamp,
            consolidated: false,
            dormant: false,
            in_progress: false,
            prompt_version: None,
            source_ids: None,
            last_decay: None,
            metadata: session_id.map(|s| serde_json::json!({"session_id": s})),
            importance: None,
            persistent: false,
            last_implicit_boost: None,
            access_count: 0,
            source: None,
            created_by: None,
            updated_at: None,
            valid_from: None,
            valid_until: None,
            is_latest: true,
            superseded_by: None,
            supersedes_id: None,
            root_memory_id: None,
            version: 1,
            memory_type: None,
            extraction_meta: None,
            deleted_at: None,
            trust_level: None,
            ingester_user_id: None,
            alpha: 1.0,
            beta: 1.0,
            harmful_count: 0,
            n_ratings: 0,
            last_rated_at: None,
        }
    }

    #[tokio::test]
    async fn co_occur_returns_sorted_results_with_session_priority() {
        use crate::auth::{AuthContext, KeyScope, UserRole};

        let (_app, state) = crate::tests::make_test_app();
        let stream = "__user_107test__";
        let auth = AuthContext::single_stream(
            stream,
            UserRole::Admin,
            KeyScope::Private,
            Some("user_107".to_string()),
            false,
        );

        let base_ts: u64 = 1_746_000_000; // arbitrary fixed epoch

        // Reference chunk: session "sess-A"
        let ref_chunk = make_chunk("ref-chunk", stream, base_ts, Some("sess-A"));

        // Same session, delta=+3600 (within window)
        let same_sess = make_chunk("same-session-chunk", stream, base_ts + 3600, Some("sess-A"));

        // Different session, delta=+60 (within window — closer but not same session)
        let diff_sess_close =
            make_chunk("diff-session-close", stream, base_ts + 60, Some("sess-B"));

        // Out of window, different session — should be excluded
        let out_of_window = make_chunk("out-of-window", stream, base_ts + 200_000, Some("sess-C")); // > 48h = 172800s

        state.store.store_chunk(&ref_chunk).expect("store ref");
        state
            .store
            .store_chunk(&same_sess)
            .expect("store same_sess");
        state
            .store
            .store_chunk(&diff_sess_close)
            .expect("store diff_sess_close");
        state
            .store
            .store_chunk(&out_of_window)
            .expect("store out_of_window");

        let params = CoOccurQuery {
            chunk_id: "ref-chunk".to_string(),
            window_hours: 48,
            top_k: 10,
        };

        let result = co_occur_handler(State(state), axum::Extension(auth), Query(params))
            .await
            .expect("handler must succeed");

        let resp = result.0;
        assert_eq!(resp.reference_chunk_id, "ref-chunk");
        assert_eq!(resp.reference_session_id.as_deref(), Some("sess-A"));

        // total_in_window = 2 (same_sess + diff_sess_close; out_of_window excluded)
        assert_eq!(
            resp.total_in_window, 2,
            "out-of-window chunk must be excluded"
        );
        assert_eq!(resp.returned, 2);

        // First result: same_session=true chunk (even though delta is larger)
        assert_eq!(
            resp.results[0].chunk_id, "same-session-chunk",
            "same_session=true must sort first"
        );
        assert!(resp.results[0].same_session);

        // Second result: diff-session-close
        assert_eq!(resp.results[1].chunk_id, "diff-session-close");
        assert!(!resp.results[1].same_session);
    }

    #[tokio::test]
    async fn co_occur_returns_404_for_soft_deleted_reference() {
        use crate::auth::{AuthContext, KeyScope, UserRole};

        let (_app, state) = crate::tests::make_test_app();
        let stream = "__user_107softdel__";
        let auth = AuthContext::single_stream(
            stream,
            UserRole::Admin,
            KeyScope::Private,
            Some("user_107sd".to_string()),
            false,
        );

        let base_ts: u64 = 1_746_100_000;
        let mut deleted_chunk = make_chunk("deleted-ref", stream, base_ts, None);
        deleted_chunk.deleted_at = Some(chrono::Utc::now().timestamp() as u64);
        state
            .store
            .store_chunk(&deleted_chunk)
            .expect("store deleted_chunk");

        let params = CoOccurQuery {
            chunk_id: "deleted-ref".to_string(),
            window_hours: 48,
            top_k: 10,
        };
        let result = co_occur_handler(State(state), axum::Extension(auth), Query(params)).await;
        assert!(
            matches!(result, Err(AppError::NotFound(_))),
            "soft-deleted reference chunk must return 404"
        );
    }

    #[tokio::test]
    async fn co_occur_skips_dormant_candidates() {
        use crate::auth::{AuthContext, KeyScope, UserRole};

        let (_app, state) = crate::tests::make_test_app();
        let stream = "__user_107dormant__";
        let auth = AuthContext::single_stream(
            stream,
            UserRole::Admin,
            KeyScope::Private,
            Some("user_107d".to_string()),
            false,
        );

        let base_ts: u64 = 1_746_200_000;

        // Reference chunk: active
        let ref_chunk = make_chunk("dormant-ref", stream, base_ts, None);

        // Dormant candidate: within window, must be skipped
        let mut dormant_cand = make_chunk("dormant-cand", stream, base_ts + 60, None);
        dormant_cand.dormant = true;

        // Active candidate: within window, must appear in results
        let active_cand = make_chunk("active-cand", stream, base_ts + 120, None);

        state.store.store_chunk(&ref_chunk).expect("store ref");
        state
            .store
            .store_chunk(&dormant_cand)
            .expect("store dormant_cand");
        state
            .store
            .store_chunk(&active_cand)
            .expect("store active_cand");

        let params = CoOccurQuery {
            chunk_id: "dormant-ref".to_string(),
            window_hours: 48,
            top_k: 10,
        };
        let result = co_occur_handler(State(state), axum::Extension(auth), Query(params))
            .await
            .expect("handler must succeed");

        let resp = result.0;
        assert_eq!(resp.results.len(), 1, "dormant candidate must be excluded");
        assert_eq!(
            resp.results[0].chunk_id, "active-cand",
            "only the active candidate must appear"
        );
    }
}
