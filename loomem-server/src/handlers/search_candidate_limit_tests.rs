//! Request-limit regressions against real tempdir RocksDB/Tantivy state.
use super::*;
use crate::auth::KeyScope;
use loomem_core::hybrid_search::{SearchCandidateBudget, VectorSearchOptions};
use loomem_core::storage::{Chunk, UserRole, DEFAULT_STREAM_ID};
use loomem_core::{SourceTag, TextDocument};
use std::collections::HashSet;

fn auth() -> AuthContext {
    AuthContext::single_stream(
        DEFAULT_STREAM_ID,
        UserRole::Admin,
        KeyScope::Shared,
        Some("test".into()),
        true,
    )
}

fn request(top_k: Option<usize>) -> SearchRequest {
    serde_json::from_value(json!({"query":"apple", "top_k":top_k, "dry_run":true}))
        .expect("request fixture")
}

fn chunk(id: &str, stream: &str, agent: &str, content: &str) -> Chunk {
    let mut chunk: Chunk = serde_json::from_value(json!({
        "id":id, "content":content, "stream":stream, "level":0,
        "score":1.0, "timestamp":1_700_000_000, "consolidated":false,
        "dormant":false, "in_progress":false,
    }))
    .expect("chunk fixture");
    chunk.source = Some(SourceTag::from_agent(agent));
    chunk
}

async fn seed_group(state: &Arc<AppState>, prefix: &str, stream: &str, agent: &str, word: &str) {
    let mut index = state.tantivy.lock().await;
    for i in 0_u16..80 {
        let id = format!("{prefix}-{i:03}");
        let chunk = chunk(&id, stream, agent, &format!("{id} {word}"));
        state.store.store_chunk(&chunk).expect("seed chunk");
        state
            .store
            .store_embedding(&id, vec![1.0, f32::from(i) / 100.0])
            .expect("seed vector");
        index
            .upsert_document(TextDocument {
                id,
                content: chunk.content,
                user_id: "test".into(),
                app_id: "test".into(),
                level: 0,
                timestamp: 1_700_000_000,
                stream: stream.into(),
                entities: None,
                relations: None,
                event_date: None,
                source_agent: Some(agent.into()),
            })
            .expect("seed index");
    }
    index.commit().expect("index commit");
}

async fn search(state: &Arc<AppState>, request: SearchRequest) -> SearchResponse {
    search_handler(State(state.clone()), axum::Extension(auth()), Json(request))
        .await
        .expect("search response")
        .0
}

#[tokio::test]
async fn request_sixty_survives_bm25_fusion_with_configured_ten() {
    let (_app, state) = crate::tests::make_test_app();
    assert_eq!(state.config.search.top_k, 10);
    seed_group(&state, "eligible", DEFAULT_STREAM_ID, "wanted", "apple").await;
    let response = search(&state, request(Some(60))).await;
    assert_eq!(
        response.results.len(),
        60,
        "eligible records must survive the old 30-item fusion cap"
    );
    let ids: HashSet<_> = response.results.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids.len(), 60);
    assert!(ids.iter().all(|id| id.starts_with("eligible-")));
}

#[tokio::test]
async fn default_limit_and_scope_are_preserved_with_larger_candidate_api() {
    let (_app, state) = crate::tests::make_test_app();
    seed_group(&state, "shared", DEFAULT_STREAM_ID, "wanted", "apple").await;
    seed_group(&state, "outside", "other-stream", "wanted", "apple").await;
    let response = search(&state, request(None)).await;
    assert_eq!(response.results.len(), 10);
    assert!(response.results.iter().all(|r| r.id.starts_with("shared-")));
    let mut explicit = request(Some(60));
    explicit.streams = Some(vec!["other-stream".into()]);
    let response = search(&state, explicit).await;
    assert_eq!(response.results.len(), 60);
    assert!(response
        .results
        .iter()
        .all(|r| r.id.starts_with("outside-")));
}

#[tokio::test]
async fn source_agent_include_and_exclude_fill_sixty_with_scope_intact() {
    let (_app, state) = crate::tests::make_test_app();
    seed_group(&state, "wanted", DEFAULT_STREAM_ID, "wanted", "apple").await;
    seed_group(
        &state,
        "decoy",
        DEFAULT_STREAM_ID,
        "decoy",
        "apple apple apple",
    )
    .await;
    seed_group(
        &state,
        "outside",
        "other-stream",
        "wanted",
        "apple apple apple",
    )
    .await;
    let mut include = request(Some(60));
    include.source_agent = Some("wanted".into());
    let mut exclude = request(Some(60));
    exclude.exclude_source_agents = Some(vec!["decoy".into()]);
    for payload in [include, exclude] {
        let response = search(&state, payload).await;
        assert_eq!(response.results.len(), 60);
        assert!(response.results.iter().all(|r| r.id.starts_with("wanted-")));
    }
}

#[tokio::test]
async fn zero_and_oversized_requests_are_rejected_and_positive_boundaries_supported() {
    let (_app, state) = crate::tests::make_test_app();
    for top_k in [0, 1001, usize::MAX] {
        let result = search_handler(
            State(state.clone()),
            axum::Extension(auth()),
            Json(request(Some(top_k))),
        )
        .await;
        assert!(
            matches!(result, Err(AppError::BadRequest(_))),
            "zero and oversized requests must fail before retrieval"
        );
    }
    seed_group(&state, "eligible", DEFAULT_STREAM_ID, "wanted", "apple").await;
    assert_eq!(search(&state, request(Some(1000))).await.results.len(), 80);
    assert_eq!(search(&state, request(Some(1))).await.results.len(), 1);
}

#[tokio::test]
async fn dense_only_pool_expands_beyond_twenty_with_real_scoped_records() {
    let (_app, state) = crate::tests::make_test_app();
    seed_group(&state, "wanted", DEFAULT_STREAM_ID, "wanted", "pear").await;
    seed_group(&state, "decoy", DEFAULT_STREAM_ID, "decoy", "pear").await;
    seed_group(&state, "outside", "other-stream", "wanted", "pear").await;
    let mut payload = request(Some(60));
    payload.source_agent = Some("wanted".into());
    let ctx = prepare_query_context(&state, &auth(), &payload).expect("query context");
    let bm25 = bm25_retrieve(&ctx, &[], &payload, &state)
        .await
        .expect("BM25");
    assert!(bm25.is_empty(), "fixture must be dense-only");
    let agents = agent_id_set(&state, &payload).await.expect("agent id set");
    let embeddings: Vec<_> = retain_by_agent_set(
        state.store.get_all_embeddings().expect("vectors"),
        Some(&agents),
    )
    .into_iter()
    .filter(|(id, _)| {
        state
            .store
            .get_chunk(id)
            .expect("read")
            .expect("chunk")
            .stream
            == DEFAULT_STREAM_ID
    })
    .collect();
    assert_eq!(embeddings.len(), 80);
    let legacy = state
        .hybrid_search
        .search_with_vector_guaranteed(
            vec![],
            &embeddings,
            &[1.0, 0.0],
            Some(&state.store),
            None,
            None,
        )
        .expect("legacy dense pool");
    assert_eq!(
        legacy.len(),
        20,
        "fixture demonstrates the configured dense fetch cap"
    );
    let expanded = state
        .hybrid_search
        .search_with_vector_budget(
            vec![],
            &embeddings,
            &[1.0, 0.0],
            VectorSearchOptions {
                store: Some(&state.store),
                stream_filter: None,
                guaranteed: None,
                budget: SearchCandidateBudget::new(state.config.search.top_k, ctx.limit)
                    .expect("budget"),
            },
        )
        .expect("expanded dense pool");
    assert_eq!(expanded.len(), 80);
    let filtered = cache_policy::truncate(filter_candidates(expanded, &ctx, &payload, &state));
    assert_eq!(filtered.results.len(), 60);
    assert!(filtered.results.iter().all(|r| r.id.starts_with("wanted-")));
    let old_ids: HashSet<_> = legacy.iter().map(|r| &r.id).collect();
    assert!(filtered.results.iter().any(|r| !old_ids.contains(&r.id)));
}

#[tokio::test]
async fn configured_request_ceiling_is_enforced_without_changing_default_ten() {
    let (_app, original) = crate::tests::make_test_app();
    let mut config = original.config.clone();
    config.search.max_request_top_k = 64;
    let (_app, state) = crate::tests::make_test_app_cfg(config);
    let result = search_handler(
        State(state.clone()),
        axum::Extension(auth()),
        Json(request(Some(65))),
    )
    .await;
    assert!(matches!(result, Err(AppError::BadRequest(_))));
    seed_group(&state, "eligible", DEFAULT_STREAM_ID, "wanted", "apple").await;
    assert_eq!(search(&state, request(Some(64))).await.results.len(), 64);
    assert_eq!(search(&state, request(None)).await.results.len(), 10);
}
