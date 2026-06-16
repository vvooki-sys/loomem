//! Hard-delete-on-demand endpoint (cycle/135, GDPR Art 17).
//!
//! `POST /v1/purge/:id` — user-triggered hard-purge for a single chunk that
//! skips the 30-day soft-delete window owned by the retention worker. Live
//! retention path (`scheduler.rs` purge worker, `config.toml` `[retention]`)
//! is unchanged; this is an orthogonal fast path.
//!
//! Contract analogous to ADR-012 (`DeleteOutcome`): `PurgeOutcome` captures
//! per-step results and maps to 200 / 207 / 404. Cascade ordering matches
//! `delete.rs::delete_memory_fully` (store → tantivy → graph) so the same
//! reorder rationale (cheap idempotent steps first; cross-reference-heavy
//! graph last + non-fatal) applies. The embedding leg is folded inside
//! `store.hard_delete_by_id` (CF_EMBEDDINGS delete) per `storage.rs`.
//!
//! Intent log uses `OpType::PurgeChunk` (cycle/135). Replay path in
//! `loomem_core::intent_log::recover` is idempotent — re-running
//! `hard_delete_by_id` + `tantivy.delete_document` against an already-purged
//! chunk is a no-op.

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use loomem_core::audit::{self, AuditEvent};
use loomem_core::graph::GraphStore;
use loomem_core::intent_log::OpType;
use loomem_core::storage::{Chunk, RocksDbStore};
use loomem_core::tantivy_index::TantivyIndex;
use serde::Deserialize;
use tokio::sync::Mutex;

use super::AppError;
use crate::auth::{AuthContext, KeyScope};
use crate::AppState;

/// Maximum allowed length of the optional `reason` field on the purge
/// request body. Audit entries embed `reason` as-is; the cap keeps a single
/// row from inflating the audit log when callers paste large blobs.
pub const MAX_REASON_LEN: usize = 500;

/// Optional payload for `POST /v1/purge/:id`. Body may be omitted entirely
/// (handler treats absent body as `PurgeRequest::default()`).
#[derive(Debug, Default, Deserialize)]
pub struct PurgeRequest {
    /// Free-text reason recorded in the audit entry. Max `MAX_REASON_LEN`
    /// chars; longer values short-circuit with HTTP 400.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Per-step outcome of `hard_delete_memory_fully`. Each step is independent;
/// the handler maps the overall shape to 200 (all OK) / 207 (partial) / 404
/// (`purge_executed == false`, chunk did not exist). Pattern mirrors
/// `delete.rs::DeleteOutcome`.
#[must_use = "PurgeOutcome captures partial-failure state — discarding it silently swallows tantivy/graph errors (cycle/135 mirrors cycle/124)"]
#[derive(Debug)]
pub struct PurgeOutcome {
    /// True iff `store.hard_delete_by_id` reported the chunk existed. Maps to
    /// HTTP 404 when false.
    pub purge_executed: bool,
    /// True iff the chunk had `deleted_at == None` before the cascade —
    /// i.e. the caller is bypassing the soft-delete window. Recorded in the
    /// audit entry so compliance can distinguish proactive purges from
    /// scheduled-retention-window purges.
    pub skipped_soft: bool,
    pub tantivy: Result<()>,
    pub graph: Result<()>,
}

impl PurgeOutcome {
    /// True when every observable step succeeded (or the chunk simply did
    /// not exist — both 404 and 200 happy paths satisfy `all_ok`).
    pub fn all_ok(&self) -> bool {
        self.tantivy.is_ok() && self.graph.is_ok()
    }
}

/// Hard-delete a chunk from every retrieval surface.
///
/// Cascade order: `store.hard_delete_by_id` (RocksDB primary + CF_EMBEDDINGS
/// + entity + relation) → `tantivy.delete_document` → `graph.remove_chunk_references`.
///
/// Returns `PurgeOutcome` capturing per-step results. Only `store.hard_delete_by_id`
/// can fail-fast and bubble `Err` from this fn (it is the existence probe);
/// tantivy/graph failures are captured inside the outcome so the handler
/// can respond 207 instead of 500 — same trade-off as
/// `delete.rs::delete_memory_fully` (cycle/117).
///
/// `skipped_soft` is derived from `existing_chunk.deleted_at` — a snapshot the
/// caller reads *before* this fn (the same probe used for RBAC). Threading it
/// in avoids a second `get_chunk`; `None` (chunk missing) → skipped_soft=false.
pub async fn hard_delete_memory_fully(
    store: &RocksDbStore,
    tantivy: &Mutex<TantivyIndex>,
    graph: &GraphStore,
    id: &str,
    existing_chunk: Option<&Chunk>,
) -> Result<PurgeOutcome> {
    // `skipped_soft` snapshot is threaded in from the caller's RBAC probe to
    // avoid a second get_chunk. `None` (chunk missing) → skipped_soft=false.
    let skipped_soft = existing_chunk.is_some_and(|c| c.deleted_at.is_none());

    let purge_executed = store
        .hard_delete_by_id(id)
        .context("store.hard_delete_by_id failed")?;

    let tantivy_result = {
        let mut idx = tantivy.lock().await;
        idx.delete_document(id)
    };
    if let Err(err) = tantivy_result.as_ref() {
        tracing::error!(
            id = %id,
            error = %err,
            "tantivy.delete_document failed after store.hard_delete_by_id — orphan in FTS, retry required"
        );
    }

    let graph_result = graph.remove_chunk_references(id);
    if let Err(err) = graph_result.as_ref() {
        tracing::error!(
            id = %id,
            error = %err,
            "graph.remove_chunk_references failed after store + tantivy purge — edges stale, retry required"
        );
    }

    Ok(PurgeOutcome {
        purge_executed,
        skipped_soft,
        tantivy: tantivy_result
            .map_err(|e| e.context("tantivy.delete_document failed after store hard-delete")),
        graph: graph_result
            .map_err(|e| e.context("graph.remove_chunk_references failed after store + tantivy")),
    })
}

/// Decide whether `auth` may purge a chunk currently owned by
/// `chunk_owner_stream`. Pure function over `(AuthContext, Option<&str>)`
/// so the auth gate is unit-testable without a live store.
///
/// * Shared scope → Admin only. Non-admin → `AppError::Forbidden`.
/// * Private scope, non-admin → owner of the chunk's stream only. Cross-stream
///   attempt → `AppError::BadRequest`.
/// * `chunk_owner_stream = None` (chunk missing) → pass; cascade then maps
///   to HTTP 404. The handler does NOT short-circuit on missing chunk here,
///   because intent log + audit log shape stays consistent across the
///   exists / missing branches.
///
/// `id` and `actor_label` exist only for the audit-log `tracing::warn!` lines.
fn enforce_purge_rbac(
    auth: &AuthContext,
    chunk_owner_stream: Option<&str>,
    id: &str,
    actor_label: &str,
) -> Result<(), AppError> {
    if auth.scope == KeyScope::Shared && !auth.role.can_delete_shared() {
        tracing::warn!(
            target: "audit",
            "PURGE denied on shared scope: user={} role={:?}",
            actor_label, auth.role
        );
        return Err(AppError::Forbidden(
            "Admin-only on shared scope: memory_purge requires Admin.".into(),
        ));
    }

    if !auth.is_admin {
        if let Some(owner) = chunk_owner_stream {
            if owner != auth.stream_id {
                tracing::warn!(
                    target: "audit",
                    "PURGE denied: user={} chunk={} owned_by={}",
                    actor_label, id, owner
                );
                return Err(AppError::BadRequest(
                    "Access denied: chunk belongs to another stream".into(),
                ));
            }
        }
    }

    Ok(())
}

/// `POST /v1/purge/:id` — user-triggered single-chunk hard-purge (cycle/135).
///
/// RBAC:
/// * Shared scope → Admin-only (`role.can_delete_shared()`). Non-admin → 403.
/// * Private scope → owner-of-stream OR Admin. Non-admin cross-stream → 400.
///
/// Body is optional `{ "reason": "<≤500 chars>" }`. Longer reason → 400.
///
/// Audit log entry is appended **after** the cascade so failed
/// short-circuits (RBAC reject, oversized reason) do not pollute the log.
/// Audit append failure is best-effort (`tracing::warn!`, not a fatal) per
/// `audit.rs` § Risks pattern.
///
/// Intent log marker uses `OpType::PurgeChunk`. Replay path is symmetric in
/// `loomem_core::intent_log::recover` (cycle/135 vs cycle/47 + /51 lesson).
pub async fn api_purge_memory_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    body: Option<Json<PurgeRequest>>,
) -> Result<(StatusCode, Json<serde_json::Value>), AppError> {
    let payload = body.map(|Json(p)| p).unwrap_or_default();

    if let Some(ref reason) = payload.reason {
        if reason.chars().count() > MAX_REASON_LEN {
            return Err(AppError::BadRequest(format!(
                "reason field exceeds {MAX_REASON_LEN} characters"
            )));
        }
    }

    let actor_label = auth.user_id.as_deref().unwrap_or("admin").to_string();
    // Single read: capture both the owner stream (for RBAC) and the chunk
    // snapshot (for skipped_soft) to avoid a second get_chunk probe inside
    // hard_delete_memory_fully.
    let existing_chunk = state.store.get_chunk(&id).ok().flatten();
    let chunk_owner_stream = existing_chunk.as_ref().map(|c| c.stream.clone());
    enforce_purge_rbac(&auth, chunk_owner_stream.as_deref(), &id, &actor_label)?;

    let intent_seq = if let Some(ref ilog) = state.intent_log {
        let mut log = ilog.lock().await;
        Some(log.append_pending(OpType::PurgeChunk, &id)?)
    } else {
        None
    };

    let outcome = hard_delete_memory_fully(
        &state.store,
        &state.tantivy,
        &state.graph,
        &id,
        existing_chunk.as_ref(),
    )
    .await?;

    // Mark WAL pending committed regardless of `purge_executed` — recovery is a
    // no-op on missing chunk (symmetric replay), and leaving the pending entry
    // would leak one WAL row per 404. Matches admin.rs OpType::Delete pattern.
    if let (Some(seq), Some(ref ilog)) = (intent_seq, &state.intent_log) {
        let mut log = ilog.lock().await;
        log.mark_committed(seq, OpType::PurgeChunk, &id)?;
    }
    if outcome.purge_executed {
        state.query_cache.lock().await.clear();
    }

    if outcome.purge_executed {
        let target_user_id = auth.user_id.clone().unwrap_or_else(|| "admin".to_string());
        let event = AuditEvent::new(
            "memory_purge",
            actor_label.clone(),
            actor_label.clone(),
            serde_json::json!({
                "chunk_id": id,
                "chunk_owner_stream": chunk_owner_stream,
                "scope": match auth.scope {
                    KeyScope::Shared => "shared",
                    KeyScope::Private => "private",
                },
                "reason": payload.reason,
                "skipped_soft": outcome.skipped_soft,
                "all_ok": outcome.all_ok(),
            }),
        );
        if let Err(e) = audit::append(&state.store, &target_user_id, &event) {
            tracing::warn!(
                "audit append failed for purge target={} chunk={}: {}",
                target_user_id,
                id,
                e
            );
        }
    }

    if !outcome.purge_executed {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"purged": false, "id": id})),
        ));
    }

    if outcome.all_ok() {
        Ok((
            StatusCode::OK,
            Json(serde_json::json!({
                "purged": true,
                "id": id,
                "skipped_soft": outcome.skipped_soft,
            })),
        ))
    } else {
        Ok((
            StatusCode::MULTI_STATUS,
            Json(serde_json::json!({
                "purged": true,
                "id": id,
                "skipped_soft": outcome.skipped_soft,
                "partial": true,
                "steps": {
                    "store": "ok",
                    "tantivy": outcome.tantivy.as_ref().err().map_or("ok", |_| "error"),
                    "graph": outcome.graph.as_ref().err().map_or("ok", |_| "error"),
                },
                "errors": {
                    "tantivy": outcome.tantivy.as_ref().err().map(std::string::ToString::to_string),
                    "graph": outcome.graph.as_ref().err().map(std::string::ToString::to_string),
                }
            })),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loomem_core::config::{RocksDbConfig, TantivyConfig};
    use loomem_core::storage::Chunk;
    use loomem_core::tantivy_index::TextDocument;
    use tempfile::TempDir;

    fn rocks_config() -> RocksDbConfig {
        RocksDbConfig {
            max_open_files: 100,
            compression: "lz4".to_string(),
            write_buffer_size: 4 * 1024 * 1024,
            max_write_buffer_number: 2,
        }
    }

    fn tantivy_config() -> TantivyConfig {
        TantivyConfig {
            enabled: true,
            heap_size_mb: 50,
            drift_warn_pct: 5.0,
            auto_rebuild_on_drift: false,
        }
    }

    fn make_chunk(id: &str, stream: &str) -> Chunk {
        Chunk {
            id: id.to_string(),
            content: format!("purge fixture {id}"),
            stream: stream.to_string(),
            level: 0,
            score: 0.5,
            timestamp: 1_700_000_000,
            consolidated: false,
            dormant: false,
            in_progress: false,
            prompt_version: None,
            source_ids: None,
            last_decay: None,
            metadata: None,
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

    fn make_text_doc(chunk: &Chunk) -> TextDocument {
        TextDocument {
            id: chunk.id.clone(),
            content: chunk.content.clone(),
            user_id: "u1".to_string(),
            app_id: "app1".to_string(),
            level: chunk.level,
            timestamp: chunk.timestamp as i64,
            stream: chunk.stream.clone(),
            entities: None,
            relations: None,
            event_date: None,
            source_agent: None,
        }
    }

    fn setup() -> (
        TempDir,
        Arc<RocksDbStore>,
        Arc<Mutex<TantivyIndex>>,
        Arc<GraphStore>,
    ) {
        let tmp = TempDir::new().unwrap();
        let store =
            Arc::new(RocksDbStore::open(tmp.path().join("rocks"), &rocks_config()).unwrap());
        let tantivy = Arc::new(Mutex::new(
            TantivyIndex::open(tmp.path().join("tantivy"), &tantivy_config()).unwrap(),
        ));
        let graph = Arc::new(GraphStore::new(store.clone()));
        (tmp, store, tantivy, graph)
    }

    /// Happy path: chunk exists and was never soft-deleted → purge_executed,
    /// skipped_soft=true, all downstream steps OK.
    #[tokio::test]
    async fn hard_delete_skip_soft_fast_path() {
        let (_tmp, store, tantivy, graph) = setup();
        let chunk = make_chunk("p-1", "__user_test__");
        store.store_chunk(&chunk).unwrap();
        store
            .store_embedding("p-1", vec![0.1f32, 0.2, 0.3])
            .unwrap();
        {
            let mut idx = tantivy.lock().await;
            idx.index_document(make_text_doc(&chunk)).unwrap();
            idx.commit().unwrap();
        }

        let pre = store.get_chunk("p-1").unwrap();
        let outcome = hard_delete_memory_fully(&store, &tantivy, &graph, "p-1", pre.as_ref())
            .await
            .unwrap();

        assert!(
            outcome.purge_executed,
            "existing chunk → purge_executed=true"
        );
        assert!(
            outcome.skipped_soft,
            "chunk had no deleted_at → skipped_soft=true"
        );
        assert!(outcome.all_ok(), "happy path → all downstream steps OK");

        // Post-condition: chunk + embedding fully gone (no soft-delete tombstone).
        assert!(store.get_chunk("p-1").unwrap().is_none());
        assert_eq!(store.count_embeddings().unwrap(), 0);
    }

    /// Chunk already soft-deleted: purge still executes but `skipped_soft=false`.
    /// Documents the audit-trail difference between proactive purges and
    /// retention-flow purges.
    #[tokio::test]
    async fn hard_delete_on_already_soft_deleted_clears_skipped_soft() {
        let (_tmp, store, tantivy, graph) = setup();
        let chunk = make_chunk("p-2", "__user_test__");
        store.store_chunk(&chunk).unwrap();
        // Soft-delete first.
        store.delete_by_id("p-2").unwrap();
        let after_soft = store.get_chunk("p-2").unwrap().unwrap();
        assert!(after_soft.deleted_at.is_some(), "soft-delete precondition");

        let outcome = hard_delete_memory_fully(&store, &tantivy, &graph, "p-2", Some(&after_soft))
            .await
            .unwrap();

        assert!(
            outcome.purge_executed,
            "soft-deleted chunk still present in store → purge_executed=true"
        );
        assert!(
            !outcome.skipped_soft,
            "deleted_at was set → skipped_soft=false"
        );
        assert!(store.get_chunk("p-2").unwrap().is_none());
    }

    /// Missing chunk: cascade still runs idempotently, purge_executed=false →
    /// handler maps to HTTP 404.
    #[tokio::test]
    async fn hard_delete_missing_chunk_returns_purge_executed_false() {
        let (_tmp, store, tantivy, graph) = setup();

        let outcome = hard_delete_memory_fully(&store, &tantivy, &graph, "never-existed", None)
            .await
            .unwrap();

        assert!(!outcome.purge_executed, "missing → purge_executed=false");
        assert!(
            !outcome.skipped_soft,
            "missing chunk → skipped_soft=false (no pre-state to snapshot)"
        );
        assert!(
            outcome.all_ok(),
            "downstream steps no-op safely on missing id"
        );
    }

    // ── RBAC unit tests via synthetic AuthContext (cycle/135 AC4) ─────────
    // make_test_app() only mints an admin-token fixture (cycle/74), so the
    // non-admin paths in `enforce_purge_rbac` are covered here instead of
    // through HTTP. AC4 split-coverage rationale matches cycle/128.

    use loomem_core::storage::UserRole;

    fn writer_auth(stream_id: &str, scope: KeyScope) -> AuthContext {
        AuthContext::single_stream(
            stream_id,
            UserRole::Writer,
            scope,
            Some("user-writer".to_string()),
            false,
        )
    }

    /// D4: non-admin caller on shared scope → Forbidden (HTTP 403 via
    /// mod.rs::IntoResponse). Owner-stream irrelevant once shared+non-admin.
    #[test]
    fn rbac_shared_non_admin_forbidden() {
        let auth = writer_auth("__shared_team__", KeyScope::Shared);
        let err = enforce_purge_rbac(&auth, Some("__shared_team__"), "chunk-x", "user-writer")
            .expect_err("shared+non-admin must reject");
        assert!(matches!(err, AppError::Forbidden(_)), "expected Forbidden");
    }

    /// D3: private scope non-admin owns their stream → pass.
    #[test]
    fn rbac_private_owner_self_passes() {
        let auth = writer_auth("__user_alice__", KeyScope::Private);
        enforce_purge_rbac(&auth, Some("__user_alice__"), "chunk-x", "alice")
            .expect("owner-self private path must pass");
    }

    /// D3: private scope non-admin tries to purge another stream's chunk →
    /// BadRequest (HTTP 400, per `delete_handler` precedent at admin.rs:222).
    #[test]
    fn rbac_private_cross_stream_non_admin_bad_request() {
        let auth = writer_auth("__user_alice__", KeyScope::Private);
        let err = enforce_purge_rbac(&auth, Some("__user_bob__"), "chunk-x", "alice")
            .expect_err("private+non-admin+cross-stream must reject");
        assert!(
            matches!(err, AppError::BadRequest(_)),
            "expected BadRequest for cross-stream"
        );
    }

    /// D5: Admin caller bypasses the stream-ownership check on private scope
    /// (handles GDPR requests filed against another user's chunks).
    #[test]
    fn rbac_admin_cross_stream_passes() {
        let auth = AuthContext::single_stream(
            "__user_admin__",
            UserRole::Admin,
            KeyScope::Private,
            Some("admin".to_string()),
            true,
        );
        enforce_purge_rbac(&auth, Some("__user_bob__"), "chunk-x", "admin")
            .expect("admin cross-stream private must pass (D5)");
    }

    /// Missing chunk (`None`) collapses to pass; cascade then maps to HTTP 404.
    #[test]
    fn rbac_missing_chunk_collapses_to_pass() {
        let auth = writer_auth("__user_alice__", KeyScope::Private);
        enforce_purge_rbac(&auth, None, "chunk-missing", "alice")
            .expect("missing chunk must pass — handler maps cascade to 404");
    }

    /// Re-purging an already-purged chunk: second call sees an empty store and
    /// reports `purge_executed=false`. Matches D8 idempotency contract.
    #[tokio::test]
    async fn hard_delete_idempotent_repeat_returns_false() {
        let (_tmp, store, tantivy, graph) = setup();
        let chunk = make_chunk("p-3", "__user_test__");
        store.store_chunk(&chunk).unwrap();

        let pre_first = store.get_chunk("p-3").unwrap();
        let first = hard_delete_memory_fully(&store, &tantivy, &graph, "p-3", pre_first.as_ref())
            .await
            .unwrap();
        assert!(first.purge_executed, "first call → purge_executed=true");

        let pre_second = store.get_chunk("p-3").unwrap();
        let second = hard_delete_memory_fully(&store, &tantivy, &graph, "p-3", pre_second.as_ref())
            .await
            .unwrap();
        assert!(
            !second.purge_executed,
            "second call must report purge_executed=false (D8 idempotency)"
        );
    }
}
