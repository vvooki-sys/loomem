//! Default-off guard for the rerank stage: with the reranker off (or the
//! request uncacheable) the stage is a pass-through — same pool, no
//! counters, no reranked cache slot — and two `/v1/search` responses built
//! from the shipped defaults and from the levers spelled out are identical.
use super::super::prepare_query_context;
use super::*;
use crate::auth::{AuthContext, KeyScope};
use crate::tests::{make_test_app, make_test_app_cfg, send};
use axum::{body::Body, http::Request};
use loomem_core::storage::{Chunk, UserRole};
use loomem_core::tantivy_index::TextDocument;
use serde_json::{json, Value};

const RECORDED: i64 = 1_704_067_200;
const STREAM: &str = "streamA";

fn auth() -> AuthContext {
    AuthContext::single_stream(STREAM, UserRole::Admin, KeyScope::Shared, None, true)
}

fn request(query: &str) -> SearchRequest {
    serde_json::from_value(json!({"query": query, "streams": [STREAM], "top_k": 10})).unwrap()
}

fn hit(id: &str, score: f64) -> HybridSearchResult {
    HybridSearchResult {
        id: id.into(),
        content: format!("lantern {id}"),
        user_id: String::new(),
        app_id: String::new(),
        level: 0,
        timestamp: RECORDED,
        score,
        bm25_score: 1.0,
        vector_score: 0.0,
        time_decay_factor: 1.0,
    }
}

fn pool() -> Vec<HybridSearchResult> {
    vec![hit("a", 0.9), hit("b", 0.8), hit("c", 0.7)]
}

fn as_json(results: &[HybridSearchResult]) -> Value {
    serde_json::to_value(results).unwrap()
}

#[test]
fn documents_are_the_raw_content_one_to_one() {
    let pool = pool();
    assert_eq!(
        documents(&pool),
        pool.iter().map(|r| r.content.clone()).collect::<Vec<_>>()
    );
    assert!(documents(&[]).is_empty());
}

#[tokio::test]
async fn stage_with_reranker_off_is_a_pass_through() {
    let (_app, state) = make_test_app();
    assert!(!state.config.search.rerank_enabled);
    let payload = request("lantern");
    let ctx = prepare_query_context(&state, &auth(), &payload).unwrap();
    let key = ctx.cache_key.expect("plain request is cacheable");
    let generation = state.query_cache.lock().await.generation();

    let input = RerankInput {
        ranked: pool(),
        final_top_k: 2,
        lane: None,
        cache_generation: generation,
    };
    let out = stage(&state, &ctx, &payload, &auth(), input).await;

    assert_eq!(as_json(&out), as_json(&pool()), "pool returned unchanged");
    let counters = state.retrieval_counters.snapshot();
    assert_eq!(counters.rerank_invocations, 0);
    assert_eq!(counters.rerank_completions, 0);
    assert!(
        state.query_cache.lock().await.get_best(key).is_none(),
        "no reranked slot published"
    );
}

#[tokio::test]
async fn stage_with_reranker_on_but_uncacheable_request_does_not_spawn() {
    let (_app, original) = make_test_app();
    let mut config = original.config.clone();
    config.search.rerank_enabled = true;
    let (_app, state) = make_test_app_cfg(config);
    // `trace` requests carry fresh pipeline metadata and are never cached.
    let mut payload = request("lantern");
    payload.trace = true;
    let ctx = prepare_query_context(&state, &auth(), &payload).unwrap();
    assert!(ctx.cache_key.is_none());

    let input = RerankInput {
        ranked: pool(),
        final_top_k: 2,
        lane: None,
        cache_generation: 0,
    };
    let out = stage(&state, &ctx, &payload, &auth(), input).await;

    assert_eq!(as_json(&out), as_json(&pool()));
    assert_eq!(state.retrieval_counters.snapshot().rerank_invocations, 0);
}

/// Drive the current-thread runtime until the spawned rerank has reported
/// a completion, without sleeping: the keyless LLM path performs no I/O,
/// so the task finishes on its first poll and one yield is enough. The
/// bound only turns a regression into a failure instead of a hang.
async fn wait_for_completion(state: &Arc<AppState>) {
    for _ in 0..1_000 {
        if state.retrieval_counters.snapshot().rerank_completions == 1 {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("async rerank never completed");
}

/// Positive path of the W0 stage: reranker on, cacheable request, no ONNX
/// model and no LLM key. The stage still returns the input unchanged (the
/// fresh response keeps the fused order), records one invocation and
/// spawns the model call. Keyless, `reranker::rerank` answers with the
/// identity order instead of an error (it maps every provider failure to
/// `Ok(identity)`; only a 2xx with malformed or truncated JSON is an
/// `Err`), so the spawn publishes a reranked slot that merely repeats the
/// fused head. The `Err` arm — no publish, no completion — needs a backend
/// stub and is covered when the synchronous rerank lands.
#[tokio::test]
async fn stage_with_reranker_on_spawns_keyless_identity_rerank() {
    let (_app, original) = make_test_app();
    let mut config = original.config.clone();
    config.search.rerank_enabled = true;
    // Never resolve a real key from the developer's environment.
    config.llm.api_key = None;
    config.llm.api_key_env = "LOOMEM_W0_TEST_KEY_THAT_IS_NEVER_SET".into();
    config.llm.providers.clear();
    config.llm.provider = "openai".into();
    assert!(
        config.llm.active_provider().is_err(),
        "test must stay keyless"
    );
    let (_app, state) = make_test_app_cfg(config);
    #[cfg(feature = "onnx-rerank")]
    assert!(state.onnx_reranker.is_none());
    let payload = request("lantern");
    let ctx = prepare_query_context(&state, &auth(), &payload).unwrap();
    let key = ctx.cache_key.expect("plain request is cacheable");
    let generation = state.query_cache.lock().await.generation();

    let input = RerankInput {
        ranked: pool(),
        final_top_k: 2,
        lane: None,
        cache_generation: generation,
    };
    let out = stage(&state, &ctx, &payload, &auth(), input).await;

    assert_eq!(as_json(&out), as_json(&pool()), "pool returned unchanged");
    assert_eq!(state.retrieval_counters.snapshot().rerank_invocations, 1);

    wait_for_completion(&state).await;
    let (is_reranked, cached) = state
        .query_cache
        .lock()
        .await
        .get_best(key)
        .expect("keyless rerank publishes the identity order");
    assert!(is_reranked);
    assert_eq!(
        cached.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        ["a", "b"],
        "identity order, cut to final_top_k"
    );
}

fn chunk(id: &str, content: &str) -> Chunk {
    serde_json::from_value(json!({
        "id": id, "content": content, "stream": STREAM,
        "level": 0, "score": 1.0, "timestamp": RECORDED,
        "consolidated": false, "dormant": false, "in_progress": false
    }))
    .unwrap()
}

/// Three chunks with distinct BM25 scores for "lantern" (term frequency and
/// length differ), so the fused order has no ties to break by chance.
async fn seed(state: &Arc<AppState>) {
    for (id, content) in [
        ("a", "lantern a"),
        ("b", "lantern lantern b bright"),
        ("c", "lantern c with several more words after it"),
    ] {
        let chunk = chunk(id, content);
        state.store.store_chunk(&chunk).unwrap();
        let mut index = state.tantivy.lock().await;
        index
            .upsert_document(TextDocument {
                id: chunk.id.clone(),
                content: chunk.content.clone(),
                user_id: "default".into(),
                app_id: "default".into(),
                level: 0,
                timestamp: RECORDED,
                stream: chunk.stream.clone(),
                entities: None,
                relations: None,
                event_date: None,
                source_agent: None,
            })
            .unwrap();
        index.commit().unwrap();
    }
}

async fn http(app: &axum::Router, query: &str) -> Value {
    let payload = json!({"query": query, "streams": [STREAM], "top_k": 10});
    let request = Request::builder()
        .method("POST")
        .uri("/v1/search")
        .header("Authorization", "Bearer admin-token")
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();
    let (status, body) = send(app.clone(), request).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let mut value: Value = serde_json::from_slice(&body).unwrap();
    value["took_ms"] = json!(0);
    value
}

fn ids(response: &Value) -> Vec<&str> {
    response["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect()
}

/// HTTP identity scaffold for waves 1-2. In W0 both arms run the same
/// effective config (defaults vs the levers spelled out at their off
/// values), so this proves only that the harness is deterministic — the
/// zero-change evidence for the extraction itself is the verbatim diff plus
/// the existing rerank_cache / cache_hit / cache_policy tests. The lever PRs
/// flip one flag in the second arm and keep the assertion where the fresh
/// response must stay identical (async-to-cache levers). Time decay is
/// switched off in both arms because it reads the wall clock in whole
/// seconds, which would make two responses differ across a second boundary
/// regardless of the levers.
#[tokio::test]
async fn http_responses_are_identical_with_every_lever_off() {
    let (_, original) = make_test_app();
    let mut base = original.config.clone();
    base.search.decay.l0_lambda = 0.0;
    base.search.decay.l1_lambda = 0.0;
    let (default_app, default_state) = make_test_app_cfg(base.clone());
    let mut config = base;
    config.search.rerank_date_prefix = false;
    config.search.rerank_llm_max_chars = 200;
    config.search.rerank_onnx_max_tokens = 0;
    config.search.rerank_sync = false;
    config.search.rerank_sync_timeout_ms = 2000;
    config.search.event_date_proximity_boost = 0.0;
    let (explicit_app, explicit_state) = make_test_app_cfg(config);
    seed(&default_state).await;
    seed(&explicit_state).await;

    for query in ["lantern", "lantern b"] {
        let expected = http(&default_app, query).await;
        let actual = http(&explicit_app, query).await;
        assert_eq!(actual, expected, "query {query:?}");
        let mut got = ids(&expected);
        got.sort_unstable();
        assert_eq!(got, ["a", "b", "c"], "query {query:?}");
    }
    assert_eq!(
        default_state
            .retrieval_counters
            .snapshot()
            .rerank_invocations,
        0
    );
}
