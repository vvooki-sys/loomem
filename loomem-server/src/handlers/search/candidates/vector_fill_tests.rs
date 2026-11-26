//! `[search] vector_fill_missing` against real tempdir RocksDB + Tantivy:
//! real BM25 retrieval, the stored embeddings, hybrid fusion with the
//! request budget, then the handler's filter + truncate. The test app has no
//! query embedder, so the query vector is given directly (as in
//! `candidate_limit_tests`).
//!
//! Fixture: 20 "dense" chunks fill the vector lane (2 x top_k 10) without
//! the query word; `target` mentions it once and has cosine ~0.89, below
//! every lane hit; 5 "lexical" chunks repeat the word but are orthogonal.
use crate::auth::{AuthContext, KeyScope};
use crate::handlers::search::{
    bm25_retrieve, cache_policy, filter_candidates, prepare_query_context,
};
use crate::handlers::types::SearchRequest;
use crate::AppState;
use loomem_core::hybrid_search::{SearchCandidateBudget, VectorSearchOptions};
use loomem_core::storage::{Chunk, UserRole, DEFAULT_STREAM_ID};
use loomem_core::{HybridSearchResult, TextDocument};
use serde_json::json;
use std::sync::Arc;

const QUERY_VECTOR: [f32; 2] = [1.0, 0.0];

fn auth() -> AuthContext {
    AuthContext::single_stream(
        DEFAULT_STREAM_ID,
        UserRole::Admin,
        KeyScope::Shared,
        Some("test".into()),
        true,
    )
}

fn request() -> SearchRequest {
    serde_json::from_value(json!({"query":"apple", "top_k":10, "dry_run":true}))
        .expect("request fixture")
}

async fn seed(state: &Arc<AppState>, id: &str, content: &str, vector: Vec<f32>) {
    let chunk: Chunk = serde_json::from_value(json!({
        "id":id, "content":content, "stream":DEFAULT_STREAM_ID, "level":0,
        "score":1.0, "timestamp":1_700_000_000, "consolidated":false,
        "dormant":false, "in_progress":false,
    }))
    .expect("chunk fixture");
    state.store.store_chunk(&chunk).expect("seed chunk");
    state
        .store
        .store_embedding(id, vector)
        .expect("seed vector");
    let mut index = state.tantivy.lock().await;
    index
        .upsert_document(TextDocument {
            id: id.into(),
            content: content.into(),
            user_id: "test".into(),
            app_id: "test".into(),
            level: 0,
            timestamp: 1_700_000_000,
            stream: DEFAULT_STREAM_ID.into(),
            entities: None,
            relations: None,
            event_date: None,
            source_agent: None,
        })
        .expect("seed index");
    index.commit().expect("index commit");
}

/// Time decay off so the wall clock cannot reorder the fixture.
async fn app(fill: bool) -> Arc<AppState> {
    let (_, original) = crate::tests::make_test_app();
    let mut config = original.config.clone();
    config.search.decay.l0_lambda = 0.0;
    config.search.decay.l1_lambda = 0.0;
    config.search.vector_fill_missing = fill;
    let (_, state) = crate::tests::make_test_app_cfg(config);
    for i in 0_u16..20 {
        let id = format!("dense-{i:02}");
        let content = format!("orchard ledger entry {i}");
        seed(&state, &id, &content, vec![1.0, f32::from(i) / 100.0]).await;
    }
    seed(
        &state,
        "target",
        "apple tart for the harvest fair",
        vec![1.0, 0.5],
    )
    .await;
    for i in 0_u16..5 {
        let id = format!("lexical-{i}");
        seed(&state, &id, "apple apple apple crate", vec![0.0, 1.0]).await;
    }
    state
}

/// Fused pool and final response ids for `apple`, top_k 10.
async fn run(state: &Arc<AppState>) -> (Vec<HybridSearchResult>, Vec<String>) {
    let payload = request();
    let ctx = prepare_query_context(state, &auth(), &payload).expect("query context");
    let bm25 = bm25_retrieve(&ctx, &[], &payload, state)
        .await
        .expect("BM25");
    assert_eq!(bm25.len(), 6, "target + 5 lexical chunks match");
    let embeddings = state.store.get_all_embeddings().expect("vectors");
    let pool = state
        .hybrid_search
        .search_with_vector_budget(
            bm25,
            &embeddings,
            &QUERY_VECTOR,
            VectorSearchOptions {
                store: Some(&state.store),
                stream_filter: None,
                guaranteed: None,
                budget: SearchCandidateBudget::new(state.config.search.top_k, ctx.limit)
                    .expect("budget"),
            },
        )
        .expect("fusion");
    let response = cache_policy::truncate(filter_candidates(pool.clone(), &ctx, &payload, state));
    let ids = response.results.iter().map(|r| r.id.clone()).collect();
    (pool, ids)
}

fn find<'a>(pool: &'a [HybridSearchResult], id: &str) -> (usize, &'a HybridSearchResult) {
    pool.iter()
        .enumerate()
        .find(|(_, r)| r.id == id)
        .expect("id in pool")
}

#[tokio::test]
async fn fill_off_scores_the_bm25_only_target_with_vector_zero() {
    let (pool, response) = run(&app(false).await).await;
    let (rank, target) = find(&pool, "target");
    assert_eq!(target.vector_score, 0.0);
    assert!(rank >= 20, "below every dense lane hit, got {rank}");
    assert!(!response.iter().any(|id| id == "target"));
}

#[tokio::test]
async fn fill_on_lifts_the_bm25_only_target_into_the_response() {
    let off_state = app(false).await;
    let on_state = app(true).await;
    let (off_pool, _) = run(&off_state).await;
    let (on_pool, response) = run(&on_state).await;
    let (rank, target) = find(&on_pool, "target");
    let cosine = loomem_core::vector_search::cosine_similarity(&QUERY_VECTOR, &[1.0, 0.5]);
    // Lane max is dense-00's cosine 1.0, so the fill is the raw cosine.
    assert_eq!(target.vector_score, cosine);
    assert_eq!(rank, 0, "real cosine + lexical match beats the dense lane");
    assert_eq!(response.first().map(String::as_str), Some("target"));
    // Orthogonal lexical chunks fill to 0.0; dense lane hits are unchanged.
    for r in &on_pool {
        let (_, before) = find(&off_pool, &r.id);
        if r.id != "target" {
            assert_eq!(r.vector_score, before.vector_score, "{}", r.id);
            assert_eq!(r.score.to_bits(), before.score.to_bits(), "{}", r.id);
        }
    }
}
