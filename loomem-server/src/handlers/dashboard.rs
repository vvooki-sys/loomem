//! Dashboard REST endpoints (embedded SPA backend).
//!
//! `GET /api/dashboard/memory` — paginated memory browser over the instance's
//! streams, and `GET /v1/memory-chain/:id` — the bitemporal version chain for
//! one chunk. Both are thin: scope resolution lives in `handlers/scope.rs`,
//! the chain walk in `loomem_core::contradiction`, and the item shape is the
//! shared `chunk_to_memory_item` (byte-identical with `GET /api/memories/:id`).

use crate::auth::AuthContext;
use crate::handlers::admin::chunk_to_memory_item;
use crate::handlers::scope::{resolve_scope, ScopeParam, ScopeResolution, Source};
use crate::handlers::AppError;
use crate::AppState;
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Query params for `GET /api/dashboard/memory`.
#[derive(Debug, Default, Deserialize)]
pub struct DashboardMemoryParams {
    #[serde(default)]
    pub scope: ScopeParam,
    pub page: Option<usize>,
    pub per_page: Option<usize>,
    pub layer: Option<String>,
    pub q: Option<String>,
    pub entity_id: Option<String>,
    pub source_agent: Option<String>,
}

/// GET /api/dashboard/memory
///
/// Paginated memory browser. Query: `?page=1&per_page=50&layer=L0|L1&q=text&
/// entity_id=<id>&source_agent=<agent>` (+ optional `scope=`, default shared —
/// the single-user dashboard reads the instance's default stream).
pub async fn dashboard_memory_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Query(params): Query<DashboardMemoryParams>,
) -> Result<Json<serde_json::Value>, AppError> {
    let resolution = resolve_scope(params.scope, &auth, &state.store)?;

    let page = params.page.unwrap_or(1).max(1);
    let per_page = params.per_page.unwrap_or(50).clamp(1, 200);
    let query_lower = params.q.as_ref().map(|q| q.to_lowercase());

    let filters = ScopedChunkFilters {
        layer: params.layer.as_deref(),
        query: query_lower.as_deref(),
        entity_id: params.entity_id.as_deref(),
        source_agent: params.source_agent.as_deref(),
    };
    let items = collect_scoped_chunks(&state, &resolution, &filters);
    let total = items.len();

    // Reverse index once across the resolved streams for the entity_ids field.
    let chunk_to_entities = build_entity_reverse_index(&state, &resolution);

    let offset = (page - 1) * per_page;
    let page_items: Vec<serde_json::Value> = items
        .into_iter()
        .skip(offset)
        .take(per_page)
        .map(|(chunk, source)| {
            let entity_ids = chunk_to_entities
                .get(&chunk.id)
                .cloned()
                .unwrap_or_default();
            chunk_to_memory_item(&chunk, source, &entity_ids)
        })
        .collect();

    Ok(Json(serde_json::json!({
        "items": page_items,
        "total": total,
        "page": page,
        "per_page": per_page,
    })))
}

/// Optional filters for [`collect_scoped_chunks`]. Bundled into a struct so
/// the function stays within the args limit as filters grow.
struct ScopedChunkFilters<'a> {
    layer: Option<&'a str>,
    query: Option<&'a str>,
    entity_id: Option<&'a str>,
    source_agent: Option<&'a str>,
}

/// True unless `filter` is set and the chunk's `source.agent` differs from it.
fn agent_matches(chunk: &loomem_core::storage::Chunk, filter: Option<&str>) -> bool {
    match filter {
        None => true,
        Some(f) => chunk.source.as_ref().is_some_and(|s| s.agent == f),
    }
}

/// Per-chunk filter shared by both scan branches: drop soft-deleted chunks and
/// those failing the layer / substring / source_agent filters. Scope
/// membership (`resolution.source_for`) stays in the caller — it yields the
/// `Source` label.
fn chunk_passes_filters(
    chunk: &loomem_core::storage::Chunk,
    filters: &ScopedChunkFilters<'_>,
) -> bool {
    if chunk.deleted_at.is_some() {
        return false;
    }
    if let Some(l) = filters.layer {
        let cl = if chunk.level == 1 { "L1" } else { "L0" };
        if cl != l {
            return false;
        }
    }
    if let Some(q) = filters.query {
        if !chunk.content.to_lowercase().contains(q) {
            return false;
        }
    }
    agent_matches(chunk, filters.source_agent)
}

/// Chunk ids referenced by `entity_id` within the resolved streams, or `None`
/// when no entity filter is set.
fn entity_target_chunk_ids(
    state: &Arc<AppState>,
    resolution: &ScopeResolution,
    entity_id: Option<&str>,
) -> Option<HashSet<String>> {
    let filter_id = entity_id?;
    let mut set = HashSet::new();
    for (_key, value) in state.store.prefix_scan(b"graph:entity:") {
        if let Ok(entity) = serde_json::from_slice::<loomem_core::graph::EntityNode>(&value) {
            if entity.id == filter_id && resolution.source_for(&entity.stream_id).is_some() {
                set.extend(entity.chunk_ids.iter().cloned());
            }
        }
    }
    Some(set)
}

/// Scan chunks across the resolved scope's streams, applying optional filters
/// (layer, substring, entity_id, source_agent). Dedup by chunk id, preferring
/// `Source::Shared` when overlapping streams yield the same chunk. Sorted by
/// `event_date` desc (from extraction_meta) with fallback to `timestamp`.
fn collect_scoped_chunks(
    state: &Arc<AppState>,
    resolution: &ScopeResolution,
    filters: &ScopedChunkFilters<'_>,
) -> Vec<(loomem_core::storage::Chunk, Source)> {
    let levels: Vec<&[u8]> = match filters.layer {
        Some("L0") => vec![b"chunk:L0:"],
        Some("L1") => vec![b"chunk:L1:"],
        _ => vec![b"chunk:L0:", b"chunk:L1:"],
    };

    let target_cids = entity_target_chunk_ids(state, resolution, filters.entity_id);

    // chunk_id → (Chunk, source). Dedup prefers Source::Shared.
    let mut by_id: HashMap<String, (loomem_core::storage::Chunk, Source)> = HashMap::new();

    if let Some(cids) = &target_cids {
        for cid in cids {
            if let Ok(Some(chunk)) = state.store.get_chunk(cid) {
                if let Some(source) = resolution.source_for(&chunk.stream) {
                    if chunk_passes_filters(&chunk, filters) {
                        merge_chunk(&mut by_id, chunk, source);
                    }
                }
            }
        }
    } else {
        for prefix in levels {
            for (_key, value) in state.store.prefix_scan(prefix) {
                if let Ok(chunk) = state.store.decode_chunk(&value) {
                    if let Some(source) = resolution.source_for(&chunk.stream) {
                        if chunk_passes_filters(&chunk, filters) {
                            merge_chunk(&mut by_id, chunk, source);
                        }
                    }
                }
            }
        }
    }

    let mut items: Vec<(loomem_core::storage::Chunk, Source)> = by_id.into_values().collect();

    items.sort_by(|(a, _), (b, _)| {
        let date_a = a
            .extraction_meta
            .as_ref()
            .and_then(|m| m.event_date.as_ref());
        let date_b = b
            .extraction_meta
            .as_ref()
            .and_then(|m| m.event_date.as_ref());
        match (date_b, date_a) {
            (Some(db), Some(da)) => db.cmp(da),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => b.timestamp.cmp(&a.timestamp),
        }
    });

    items
}

/// Insert chunk into the dedup map, keeping `Source::Shared` if we see the
/// same chunk id twice.
fn merge_chunk(
    by_id: &mut HashMap<String, (loomem_core::storage::Chunk, Source)>,
    chunk: loomem_core::storage::Chunk,
    source: Source,
) {
    match by_id.get(&chunk.id) {
        Some((_, Source::Shared)) => {} // keep existing shared
        _ => {
            by_id.insert(chunk.id.clone(), (chunk, source));
        }
    }
}

/// Build `chunk_id → [entity_id]` across the resolved scope's streams, used
/// to annotate list rows with their entity memberships.
fn build_entity_reverse_index(
    state: &Arc<AppState>,
    resolution: &ScopeResolution,
) -> HashMap<String, Vec<String>> {
    let mut chunk_to_entities: HashMap<String, Vec<String>> = HashMap::new();
    for (_key, value) in state.store.prefix_scan(b"graph:entity:") {
        if let Ok(entity) = serde_json::from_slice::<loomem_core::graph::EntityNode>(&value) {
            if resolution.source_for(&entity.stream_id).is_none() {
                continue;
            }
            for cid in &entity.chunk_ids {
                chunk_to_entities
                    .entry(cid.clone())
                    .or_default()
                    .push(entity.id.clone());
            }
        }
    }
    chunk_to_entities
}

/// Query params for `GET /v1/memory-chain/:id`.
#[derive(Debug, Default, Deserialize)]
pub struct MemoryChainParams {
    pub limit: Option<usize>,
}

/// GET /v1/memory-chain/:id
///
/// The bitemporal version chain (root → … → latest) for one chunk. Ownership
/// follows the same rule as `GET /api/memories/:id`: non-admin callers may
/// only read chains rooted in their own stream.
pub async fn memory_chain_handler(
    State(state): State<Arc<AppState>>,
    Path(chunk_id): Path<String>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Query(params): Query<MemoryChainParams>,
) -> Result<Json<serde_json::Value>, AppError> {
    let limit = params.limit.unwrap_or(20).min(100);

    let chain = loomem_core::contradiction::get_memory_chain(&state.store, &chunk_id, limit)?;

    if let Some(first) = chain.first() {
        if !auth.is_admin && first.stream != auth.stream_id {
            return Err(AppError::Forbidden("access denied".into()));
        }
    }

    let chain_json: Vec<serde_json::Value> = chain
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "content": c.content,
                "version": c.version,
                "is_latest": c.is_latest,
                "supersedes_id": c.supersedes_id,
                "superseded_by": c.superseded_by,
                "root_memory_id": c.root_memory_id,
                "timestamp": c.timestamp,
                "memory_type": c.memory_type,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "chunk_id": chunk_id,
        "chain_length": chain_json.len(),
        "chain": chain_json,
    })))
}

#[cfg(test)]
mod tests {
    //! HTTP integration tests over the real router + a tempdir RocksDB store
    //! (`crate::tests::make_test_app`) — no storage mocks.

    use crate::tests::{make_test_app, send};
    use axum::body::Body;
    use axum::http::{header, Method, Request, StatusCode};
    use loomem_core::storage::{Chunk, ProvenanceRole, DEFAULT_STREAM_ID};

    fn fixture_chunk(id: &str, content: &str, level: i32) -> Chunk {
        Chunk {
            id: id.to_string(),
            content: content.to_string(),
            stream: DEFAULT_STREAM_ID.to_string(),
            level,
            score: 0.9,
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
            provenance_role: ProvenanceRole::Claim,
        }
    }

    fn get(uri: &str) -> Request<Body> {
        Request::builder()
            .method(Method::GET)
            .uri(uri)
            .header(header::AUTHORIZATION, "Bearer admin-token")
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn memory_list_returns_items_with_pagination_shape() {
        let (app, state) = make_test_app();
        state
            .store
            .store_chunk(&fixture_chunk("dash-a", "alpha fact", 0))
            .unwrap();
        state
            .store
            .store_chunk(&fixture_chunk("dash-b", "beta fact", 1))
            .unwrap();

        let (status, body) = send(app, get("/api/dashboard/memory")).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["total"], serde_json::json!(2));
        assert_eq!(v["page"], serde_json::json!(1));
        assert_eq!(v["per_page"], serde_json::json!(50));
        let items = v["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        // Row shape matches GET /api/memories/:id (shared chunk_to_memory_item).
        assert!(items[0].get("layer").is_some());
        assert!(items[0].get("entity_ids").is_some());
        assert!(items[0].get("source").is_some());
    }

    #[tokio::test]
    async fn memory_list_filters_by_substring_and_layer() {
        let (app, state) = make_test_app();
        state
            .store
            .store_chunk(&fixture_chunk("dash-a", "alpha fact", 0))
            .unwrap();
        state
            .store
            .store_chunk(&fixture_chunk("dash-b", "beta fact", 1))
            .unwrap();

        let (status, body) = send(app.clone(), get("/api/dashboard/memory?q=beta")).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["total"], serde_json::json!(1));
        assert_eq!(v["items"][0]["id"], serde_json::json!("dash-b"));

        let (status, body) = send(app, get("/api/dashboard/memory?layer=L1")).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["total"], serde_json::json!(1));
        assert_eq!(v["items"][0]["layer"], serde_json::json!("L1"));
    }

    #[tokio::test]
    async fn memory_list_excludes_soft_deleted() {
        let (app, state) = make_test_app();
        let mut dead = fixture_chunk("dash-dead", "soft deleted fact", 0);
        dead.deleted_at = Some(1_700_000_001);
        state.store.store_chunk(&dead).unwrap();

        let (status, body) = send(app, get("/api/dashboard/memory")).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["total"], serde_json::json!(0));
    }

    #[tokio::test]
    async fn memory_list_requires_auth() {
        let (app, _state) = make_test_app();
        let req = Request::builder()
            .method(Method::GET)
            .uri("/api/dashboard/memory")
            .body(Body::empty())
            .unwrap();
        let (status, _) = send(app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn memory_chain_walks_versions_root_to_latest() {
        let (app, state) = make_test_app();
        let mut v1 = fixture_chunk("chain-v1", "first version", 0);
        v1.is_latest = false;
        v1.superseded_by = Some("chain-v2".into());
        let mut v2 = fixture_chunk("chain-v2", "second version", 0);
        v2.version = 2;
        v2.supersedes_id = Some("chain-v1".into());
        state.store.store_chunk(&v1).unwrap();
        state.store.store_chunk(&v2).unwrap();

        // Asking for either end of the chain yields the same root→latest walk.
        let (status, body) = send(app, get("/v1/memory-chain/chain-v2")).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["chain_length"], serde_json::json!(2));
        let chain = v["chain"].as_array().unwrap();
        assert_eq!(chain[0]["id"], serde_json::json!("chain-v1"));
        assert_eq!(chain[1]["id"], serde_json::json!("chain-v2"));
        assert_eq!(chain[1]["is_latest"], serde_json::json!(true));
        assert_eq!(chain[1]["supersedes_id"], serde_json::json!("chain-v1"));
    }

    #[tokio::test]
    async fn memory_chain_unknown_id_yields_empty_chain() {
        let (app, _state) = make_test_app();
        let (status, body) = send(app, get("/v1/memory-chain/no-such-id")).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["chain_length"], serde_json::json!(0));
    }
}
