//! Authoritative visibility checks before returning cached source text.
use std::sync::Arc;

use loomem_core::{storage::Chunk, HybridSearchResult};
use serde_json::json;

use super::{QueryContext, SearchRequest, SearchResult};
use crate::AppState;

pub(super) fn results(
    cached: &[HybridSearchResult],
    ctx: &QueryContext,
    payload: &SearchRequest,
    state: &Arc<AppState>,
    is_reranked: bool,
) -> Vec<SearchResult> {
    cached
        .iter()
        .filter_map(|result| {
            // A stale cache entry or in-flight reinsert never overrides a tombstone.
            let chunk = state.store.get_chunk(&result.id).ok().flatten()?;
            visible(&chunk, ctx, payload).then(|| serialize(result, state, is_reranked))
        })
        .collect()
}

fn visible(chunk: &Chunk, ctx: &QueryContext, payload: &SearchRequest) -> bool {
    chunk.deleted_at.is_none()
        && !chunk.dormant
        && !chunk.in_progress
        && ctx
            .stream_list
            .as_ref()
            .is_none_or(|streams| streams.contains(&chunk.stream))
        && (payload.include_superseded || (chunk.is_latest && chunk.superseded_by.is_none()))
}

fn serialize(result: &HybridSearchResult, state: &AppState, is_reranked: bool) -> SearchResult {
    let content_type = loomem_core::content_type::get_content_type(&state.store, &result.id);
    SearchResult {
        id: result.id.clone(),
        content: result.content.clone(),
        score: result.score,
        metadata: Some(json!({
            "level": result.level,
            "timestamp": result.timestamp,
            "bm25_score": result.bm25_score,
            "vector_score": result.vector_score,
            "time_decay": result.time_decay_factor,
            "cached": true,
            "reranked": is_reranked,
        })),
        trace_info: None,
        signal_breakdown: None,
        content_type: content_type.map(|value| value.content_type.as_str().to_string()),
        content_type_source: content_type.map(|value| value.source.as_str().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthContext, KeyScope, UserRole};
    use crate::handlers::search::stream_scoping_e2e_tests::seed_one;

    fn cached(id: &str) -> HybridSearchResult {
        HybridSearchResult {
            id: id.into(),
            content: format!("cached evidence projection {id}"),
            user_id: String::new(),
            app_id: String::new(),
            level: 0,
            timestamp: 0,
            score: 1.0,
            bm25_score: 1.0,
            vector_score: 0.0,
            time_decay_factor: 1.0,
        }
    }

    #[tokio::test]
    async fn late_stale_reinsert_cannot_restore_deleted_missing_or_cross_scope_text() {
        let (_, state) = crate::tests::make_test_app();
        seed_one(
            &state,
            "alive",
            "streamA",
            "synthetic live cached source",
            None,
        )
        .await;
        seed_one(
            &state,
            "deleted",
            "streamA",
            "synthetic deleted cached source",
            None,
        )
        .await;
        seed_one(
            &state,
            "outside",
            "streamB",
            "synthetic outside cached source",
            None,
        )
        .await;
        let payload: SearchRequest =
            serde_json::from_value(json!({"query":"synthetic", "streams":["streamA"]})).unwrap();
        let auth =
            AuthContext::single_stream("streamA", UserRole::Admin, KeyScope::Shared, None, true);
        let ctx = super::super::prepare_query_context(&state, &auth, &payload).unwrap();
        state.store.delete_by_id("deleted").unwrap();
        let late_results = vec![
            cached("alive"),
            cached("deleted"),
            cached("outside"),
            cached("missing"),
        ];
        let mut cache = state.query_cache.lock().await;
        cache.insert(1, late_results.clone());
        cache.insert_reranked(1, late_results);
        let (_, stale) = cache.get_best(1).unwrap();
        drop(cache);
        let response = results(&stale, &ctx, &payload, &state, true);
        assert_eq!(response.len(), 1);
        assert_eq!(response[0].id, "alive");
        assert_eq!(response[0].content, "cached evidence projection alive");
        assert_eq!(response[0].metadata.as_ref().unwrap()["reranked"], true);
    }
}
