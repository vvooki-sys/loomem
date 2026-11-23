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

/// Flag off: the raw content one to one, even for ids the store does not
/// hold (the store is never read) — the pre-flag documents byte for byte.
#[test]
fn documents_with_date_prefix_off_are_the_raw_content_one_to_one() {
    let (_app, state) = make_test_app();
    assert!(!state.config.search.rerank_date_prefix);
    let pool = pool();
    assert!(state.store.get_chunk("a").unwrap().is_none());
    assert_eq!(
        documents(&pool, &state.store, &state.config.search),
        pool.iter().map(|r| r.content.clone()).collect::<Vec<_>>()
    );
    assert!(documents(&[], &state.store, &state.config.search).is_empty());
}

fn date_prefix_on() -> SearchConfig {
    let (_app, state) = make_test_app();
    let mut search = state.config.search.clone();
    search.rerank_date_prefix = true;
    search
}

fn event_meta(
    content: &str,
    event_date: &str,
    source_id: Option<String>,
) -> loomem_core::storage::ExtractionMeta {
    loomem_core::memory_extractor::ExtractedFact {
        content: content.into(),
        fact_type: "event".into(),
        subject: None,
        event_date: Some(event_date.into()),
        event_date_context: None,
        confidence: 0.9,
        attributed_to: None,
    }
    .to_extraction_meta(source_id, "test")
}

/// A legacy chunk with an extracted event date (no ingest marker).
fn legacy_event_chunk(id: &str, event_date: &str) -> Chunk {
    let mut chunk = chunk(id, &format!("lantern {id}"));
    chunk.extraction_meta = Some(event_meta(&chunk.content, event_date, None));
    chunk
}

const SOURCE_ID: &str = "00000000-0000-4000-8000-000000000001";

/// The server-owned `memory_ingest` source a fact links to (the store
/// refuses a fact whose lineage ancestor is missing).
fn ingest_source_chunk() -> Chunk {
    let mut chunk = chunk(SOURCE_ID, "raw transcript");
    chunk.metadata = Some(json!({"_loomem_ingest": {
        "version": 1, "kind": "source", "conversation_date": "2026-10-20"
    }}));
    chunk.consolidated = true;
    chunk.trust_level = Some("a2".into());
    chunk.persistent = true;
    chunk.provenance_role = loomem_core::storage::ProvenanceRole::Evidence;
    chunk.source = Some(loomem_core::source_tag::SourceTag::from_agent(
        "mcp-ingest-source",
    ));
    chunk
}

/// A server-owned `memory_ingest` fact (marker + linked source), the shape
/// `memory_search` renders as `[event:YYYY-MM-DD]`.
fn ingest_fact_chunk(id: &str, event_date: &str) -> Chunk {
    let mut chunk = chunk(id, &format!("lantern {id}"));
    let source_id = SOURCE_ID.to_string();
    chunk.metadata = Some(json!({"_loomem_ingest": {
        "version": 1, "kind": "fact", "conversation_date": "2026-10-20"
    }}));
    chunk.level = 1;
    chunk.trust_level = Some("a2".into());
    chunk.persistent = true;
    chunk.source_ids = Some(vec![source_id.clone()]);
    chunk.source = Some(loomem_core::source_tag::SourceTag::from_agent("mcp-ingest"));
    chunk.extraction_meta = Some(event_meta(&chunk.content, event_date, Some(source_id)));
    chunk
}

/// Flag on: each document carries the `memory_search` date — the extracted
/// event date of a legacy chunk, the `event:` label of an ingest fact, and
/// the hit's own timestamp when the store has no chunk for the id.
#[test]
fn documents_with_date_prefix_on_carry_the_memory_search_date() {
    let (_app, state) = make_test_app();
    let legacy = legacy_event_chunk("a", "2023-05-06");
    let ingest = ingest_fact_chunk("b", "2022-02-03");
    assert!(crate::handlers::ingest_date::new_ingest_date_prefix(&legacy).is_none());
    assert!(crate::handlers::ingest_date::new_ingest_date_prefix(&ingest).is_some());
    state.store.store_chunk(&legacy).unwrap();
    state.store.store_chunk(&ingest_source_chunk()).unwrap();
    state.store.store_chunk(&ingest).unwrap();
    assert!(state.store.get_chunk("c").unwrap().is_none());

    let docs = documents(&pool(), &state.store, &date_prefix_on());

    assert_eq!(
        docs,
        [
            "[2023-05-06] lantern a",
            "[event:2022-02-03] lantern b",
            "[2024-01-01] lantern c",
        ]
    );
}

/// A timestamp that maps to no date (and no chunk) leaves the bare content
/// rather than an empty `[] ` prefix.
#[test]
fn documents_with_date_prefix_on_and_no_date_keep_the_bare_content() {
    let (_app, state) = make_test_app();
    let mut orphan = hit("x", 0.5);
    orphan.timestamp = i64::MAX;
    let docs = documents(&[orphan], &state.store, &date_prefix_on());
    assert_eq!(docs, ["lantern x"]);
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
    wait_for_completions(state, 1).await;
}

async fn wait_for_completions(state: &Arc<AppState>, expected: u64) {
    for _ in 0..1_000 {
        if state.retrieval_counters.snapshot().rerank_completions == expected {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("async rerank never completed");
}

/// Reranker on, keyless (the developer's real key can never be resolved).
fn keyless_rerank_config(mut config: loomem_core::config::Config) -> loomem_core::config::Config {
    config.search.rerank_enabled = true;
    config.llm.api_key = None;
    config.llm.api_key_env = "LOOMEM_TEST_KEY_THAT_IS_NEVER_SET".into();
    config.llm.providers.clear();
    config.llm.provider = "openai".into();
    assert!(
        config.llm.active_provider().is_err(),
        "test must stay keyless"
    );
    config
}

/// Positive path of the W0 stage: reranker on, cacheable request, no ONNX
/// model and no LLM key. The stage still returns the input unchanged (the
/// fresh response keeps the fused order), records one invocation and
/// spawns the model call. Keyless, `reranker::rerank` answers with the
/// identity order instead of an error (it maps every provider failure to
/// `Ok(identity)`; only a 2xx with malformed or truncated JSON is an
/// `Err`), so the spawn publishes a reranked slot that merely repeats the
/// fused head. The `Err` arm — no publish, no completion — is covered for
/// the synchronous rerank with a backend stub (`sync_model_error_…` below).
#[tokio::test]
async fn stage_with_reranker_on_spawns_keyless_identity_rerank() {
    let (_app, original) = make_test_app();
    // Never resolve a real key from the developer's environment.
    let config = keyless_rerank_config(original.config.clone());
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

/// W2b levers on (date prefix, a smaller LLM cap, an ONNX token cap) with
/// the reranker on and keyless: the rerank stays async-to-cache, so the
/// fresh responses are identical to the levers-off arm, and so are the
/// cache hits served after the spawned reranks complete (keyless, both
/// arms publish the identity order whatever the document text).
#[tokio::test]
async fn http_responses_are_identical_with_document_text_levers_on() {
    let (_, original) = make_test_app();
    let mut base = keyless_rerank_config(original.config.clone());
    base.search.decay.l0_lambda = 0.0;
    base.search.decay.l1_lambda = 0.0;
    let (off_app, off_state) = make_test_app_cfg(base.clone());
    let mut config = base;
    config.search.rerank_date_prefix = true;
    config.search.rerank_llm_max_chars = 50;
    config.search.rerank_onnx_max_tokens = 256;
    let (on_app, on_state) = make_test_app_cfg(config);
    #[cfg(feature = "onnx-rerank")]
    assert!(off_state.onnx_reranker.is_none() && on_state.onnx_reranker.is_none());
    seed(&off_state).await;
    seed(&on_state).await;

    let queries = ["lantern", "lantern b"];
    for query in queries {
        let expected = http(&off_app, query).await;
        assert_eq!(http(&on_app, query).await, expected, "fresh {query:?}");
    }
    wait_for_completions(&off_state, 2).await;
    wait_for_completions(&on_state, 2).await;
    for query in queries {
        let expected = http(&off_app, query).await;
        assert_eq!(http(&on_app, query).await, expected, "cached {query:?}");
    }
    assert_eq!(on_state.retrieval_counters.snapshot().rerank_invocations, 2);
}

/// Local keyless chat-completions mock: forwards each request body to the
/// test and answers with a fixed reranker reply.
async fn fake_rerank_provider() -> (String, tokio::sync::mpsc::UnboundedReceiver<Value>) {
    use axum::{extract::State, routing::post, Json};
    async fn complete(
        State(tx): State<tokio::sync::mpsc::UnboundedSender<Value>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        tx.send(body).unwrap();
        Json(json!({"choices": [{
            "finish_reason": "stop",
            "message": {"content": "{\"indices\": [1, 0]}"}
        }]}))
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let mock = axum::Router::new()
        .route("/v1/chat/completions", post(complete))
        .with_state(tx);
    tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    (base_url, rx)
}

/// Reranker on against the local mock: a provider entry without
/// `api_key_env`, so no key is read from the environment and no
/// `Authorization` header is sent; nothing leaves 127.0.0.1.
fn local_rerank_config(base_url: &str) -> loomem_core::config::Config {
    let (_, original) = make_test_app();
    let mut config = keyless_rerank_config(original.config.clone());
    config.llm.providers.insert(
        "local-fake".into(),
        serde_json::from_value(json!({"base_url": base_url})).unwrap(),
    );
    config.llm.provider = "local-fake".into();
    let provider = config.llm.active_provider().expect("local provider");
    assert!(
        provider.api_key.is_none(),
        "mock provider must stay keyless"
    );
    assert!(provider.base_url.starts_with("http://127.0.0.1:"));
    config
}

/// The levers reach the model: with the date prefix on and a 16-character
/// LLM cap, the prompt the reranker sends numbers the dated, capped
/// documents (`[i] [YYYY-MM-DD] lan`), and the reply's order lands in the
/// reranked cache slot.
#[tokio::test]
async fn llm_prompt_carries_dated_capped_documents() {
    let (base_url, mut requests) = fake_rerank_provider().await;
    let mut config = local_rerank_config(&base_url);
    config.search.rerank_date_prefix = true;
    config.search.rerank_llm_max_chars = 16;
    let (_app, state) = make_test_app_cfg(config);
    #[cfg(feature = "onnx-rerank")]
    assert!(state.onnx_reranker.is_none());
    state
        .store
        .store_chunk(&legacy_event_chunk("a", "2023-05-06"))
        .unwrap();
    let payload = request("lantern");
    let ctx = prepare_query_context(&state, &auth(), &payload).unwrap();
    let key = ctx.cache_key.expect("plain request is cacheable");
    let generation = state.query_cache.lock().await.generation();
    let input = RerankInput {
        ranked: vec![hit("a", 0.9), hit("b", 0.8)],
        final_top_k: 2,
        lane: None,
        cache_generation: generation,
    };
    stage(&state, &ctx, &payload, &auth(), input).await;

    let deadline = std::time::Duration::from_secs(5);
    let body = tokio::time::timeout(deadline, requests.recv())
        .await
        .expect("reranker called the mock")
        .unwrap();
    let prompt = body["messages"][1]["content"].as_str().unwrap();
    assert!(
        prompt.contains("Results:\n[0] [2023-05-06] lan\n\n[1] [2024-01-01] lan\n\n"),
        "{prompt}"
    );
    // Real loopback I/O: spin (no sleep) until the reply is processed,
    // bounded by the deadline rather than a yield count.
    let completed = async {
        while state.retrieval_counters.snapshot().rerank_completions == 0 {
            tokio::task::yield_now().await;
        }
    };
    tokio::time::timeout(deadline, completed)
        .await
        .expect("rerank completed");
    let (is_reranked, cached) = state.query_cache.lock().await.get_best(key).unwrap();
    assert!(is_reranked);
    let ids: Vec<_> = cached.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["b", "a"], "mock order published");
}

// ---------------------------------------------------------------------------
// W2a — synchronous rerank (`[search] rerank_sync`).
// ---------------------------------------------------------------------------

/// Backend stubs for [`sync_rerank`]: a fixed order, a model failure, and a
/// call that never returns (only the timeout can end it).
enum Stub {
    Order(Vec<usize>),
    Fail,
    Hang,
}

impl RerankBackend for Stub {
    async fn run_model(
        self,
        _query: String,
        _documents: Vec<String>,
        _top_k: usize,
    ) -> anyhow::Result<Vec<usize>> {
        match self {
            Stub::Order(order) => Ok(order),
            Stub::Fail => Err(anyhow::anyhow!("stub model failure")),
            Stub::Hang => std::future::pending().await,
        }
    }
}

/// Reranker on, synchronous, keyless.
fn sync_state(timeout_ms: u64) -> Arc<AppState> {
    let (_, original) = make_test_app();
    let mut config = keyless_rerank_config(original.config.clone());
    config.search.rerank_sync = true;
    config.search.rerank_sync_timeout_ms = timeout_ms;
    let (_app, state) = make_test_app_cfg(config);
    state
}

/// A cacheable request, its context, cache key and the cache generation.
async fn cacheable(state: &Arc<AppState>) -> (SearchRequest, QueryContext, u64, u64) {
    let payload = request("lantern");
    let ctx = prepare_query_context(state, &auth(), &payload).unwrap();
    let key = ctx.cache_key.expect("plain request is cacheable");
    let generation = state.query_cache.lock().await.generation();
    (payload, ctx, key, generation)
}

fn input(final_top_k: usize, cache_generation: u64) -> RerankInput<'static> {
    RerankInput {
        ranked: pool(),
        final_top_k,
        lane: None,
        cache_generation,
    }
}

fn result_ids(results: &[HybridSearchResult]) -> Vec<&str> {
    results.iter().map(|r| r.id.as_str()).collect()
}

/// `(rerank_invocations, rerank_completions)`.
fn counts(state: &AppState) -> (u64, u64) {
    let snapshot = state.retrieval_counters.snapshot();
    (snapshot.rerank_invocations, snapshot.rerank_completions)
}

/// Success with a cache key: the model's order is the fresh result (cut to
/// `final_top_k`), and the same order lands in the reranked cache slot.
#[tokio::test]
async fn sync_success_returns_model_order_and_publishes_it() {
    let state = sync_state(2000);
    let (payload, _ctx, key, generation) = cacheable(&state).await;

    let out = sync_rerank(
        &state,
        &payload,
        input(3, generation),
        Some(key),
        Stub::Order(vec![2, 0, 1]),
    )
    .await;

    assert_eq!(result_ids(&out), ["c", "a", "b"]);
    assert_eq!(counts(&state), (1, 1));
    let (is_reranked, cached) = state.query_cache.lock().await.get_best(key).unwrap();
    assert!(is_reranked);
    assert_eq!(as_json(&cached), as_json(&out), "published == returned");
}

/// The fresh result keeps the pool's membership: cut to `final_top_k`, and
/// ids the model left out are filled in fused order.
#[tokio::test]
async fn sync_success_is_cut_to_final_top_k_and_filled_in_fused_order() {
    let state = sync_state(2000);
    let (payload, _ctx, key, generation) = cacheable(&state).await;
    let out = sync_rerank(
        &state,
        &payload,
        input(2, generation),
        Some(key),
        Stub::Order(vec![2, 0, 1]),
    )
    .await;
    assert_eq!(result_ids(&out), ["c", "a"]);

    let state = sync_state(2000);
    let (payload, _ctx, key, generation) = cacheable(&state).await;
    let out = sync_rerank(
        &state,
        &payload,
        input(3, generation),
        Some(key),
        Stub::Order(vec![1]),
    )
    .await;
    assert_eq!(result_ids(&out), ["b", "a", "c"]);
}

/// No cache key (uncacheable request): reranked all the same, nothing
/// published.
#[tokio::test]
async fn sync_success_without_cache_key_reranks_but_does_not_publish() {
    let state = sync_state(2000);
    let (payload, _ctx, key, generation) = cacheable(&state).await;

    let out = sync_rerank(
        &state,
        &payload,
        input(3, generation),
        None,
        Stub::Order(vec![2, 0, 1]),
    )
    .await;

    assert_eq!(result_ids(&out), ["c", "a", "b"]);
    assert_eq!(counts(&state), (1, 1));
    assert!(state.query_cache.lock().await.get_best(key).is_none());
}

/// Model error: the fused pool comes back untouched (not even cut), nothing
/// is published, no completion, and no async rerank is started instead.
#[tokio::test]
async fn sync_model_error_keeps_fused_order_without_fallback_spawn() {
    let state = sync_state(2000);
    let (payload, _ctx, key, generation) = cacheable(&state).await;

    let out = sync_rerank(
        &state,
        &payload,
        input(2, generation),
        Some(key),
        Stub::Fail,
    )
    .await;

    assert_eq!(as_json(&out), as_json(&pool()));
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(counts(&state), (1, 0), "one call, no completion, no spawn");
    assert!(state.query_cache.lock().await.get_best(key).is_none());
}

/// Timeout on a paused clock: still pending 1 ms before the budget, the
/// fused pool just past it (the timer wheel rounds a deadline up to the next
/// whole millisecond); nothing published, no async fallback.
#[tokio::test]
async fn sync_timeout_keeps_fused_order_without_fallback_spawn() {
    const BUDGET_MS: u64 = 50;
    let state = sync_state(BUDGET_MS);
    let (payload, _ctx, key, generation) = cacheable(&state).await;
    tokio::time::pause();

    let call = sync_rerank(
        &state,
        &payload,
        input(2, generation),
        Some(key),
        Stub::Hang,
    );
    tokio::pin!(call);
    assert!(futures::poll!(&mut call).is_pending());
    tokio::time::advance(std::time::Duration::from_millis(BUDGET_MS - 1)).await;
    assert!(
        futures::poll!(&mut call).is_pending(),
        "budget not spent yet"
    );
    tokio::time::advance(std::time::Duration::from_millis(2)).await;
    let std::task::Poll::Ready(out) = futures::poll!(&mut call) else {
        panic!("sync rerank must give up at the budget");
    };

    assert_eq!(as_json(&out), as_json(&pool()));
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(counts(&state), (1, 0), "one call, no completion, no spawn");
    assert!(state.query_cache.lock().await.get_best(key).is_none());
}

/// `rerank_sync` sits behind the same gate: reranker off → pass-through,
/// no model call.
#[tokio::test]
async fn stage_with_rerank_sync_but_reranker_off_is_a_pass_through() {
    let (_, original) = make_test_app();
    let mut config = original.config.clone();
    config.search.rerank_sync = true;
    assert!(!config.search.rerank_enabled);
    let (_app, state) = make_test_app_cfg(config);
    let (payload, ctx, key, generation) = cacheable(&state).await;

    let out = stage(&state, &ctx, &payload, &auth(), input(2, generation)).await;

    assert_eq!(as_json(&out), as_json(&pool()));
    assert_eq!(counts(&state), (0, 0));
    assert!(state.query_cache.lock().await.get_best(key).is_none());
}

/// Stage with `rerank_sync` on, keyless, no ONNX model: the LLM reranker
/// answers with the identity order (provider failures map to
/// `Ok(identity)`), so the fresh result is the fused head cut to
/// `final_top_k`, the reranked slot is already there when `stage` returns,
/// and exactly one model call ran (no async spawn on top).
#[tokio::test]
async fn stage_with_rerank_sync_keyless_reranks_inline_to_identity() {
    let state = sync_state(2000);
    #[cfg(feature = "onnx-rerank")]
    assert!(state.onnx_reranker.is_none());
    let (payload, ctx, key, generation) = cacheable(&state).await;

    let out = stage(&state, &ctx, &payload, &auth(), input(2, generation)).await;

    assert_eq!(result_ids(&out), ["a", "b"]);
    assert_eq!(counts(&state), (1, 1));
    let (is_reranked, cached) = state.query_cache.lock().await.get_best(key).unwrap();
    assert!(is_reranked);
    assert_eq!(result_ids(&cached), ["a", "b"]);
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(counts(&state), (1, 1), "no async rerank on top");
}

/// HTTP, reranker on, keyless, no model: `rerank_sync = true` gives the
/// same fresh responses as the async path, and the same cache hits.
#[tokio::test]
async fn http_rerank_sync_keyless_is_identical_to_async() {
    let (_, original) = make_test_app();
    let mut base = keyless_rerank_config(original.config.clone());
    base.search.decay.l0_lambda = 0.0;
    base.search.decay.l1_lambda = 0.0;
    let (off_app, off_state) = make_test_app_cfg(base.clone());
    let mut config = base;
    config.search.rerank_sync = true;
    assert!(config.llm.active_provider().is_err());
    let (on_app, on_state) = make_test_app_cfg(config);
    #[cfg(feature = "onnx-rerank")]
    assert!(off_state.onnx_reranker.is_none() && on_state.onnx_reranker.is_none());
    seed(&off_state).await;
    seed(&on_state).await;

    let queries = ["lantern", "lantern b"];
    for query in queries {
        let expected = http(&off_app, query).await;
        assert_eq!(http(&on_app, query).await, expected, "fresh {query:?}");
    }
    assert_eq!(counts(&on_state), (2, 2), "sync reranks done inline");
    wait_for_completions(&off_state, 2).await;
    for query in queries {
        let expected = http(&off_app, query).await;
        assert_eq!(http(&on_app, query).await, expected, "cached {query:?}");
    }
    assert_eq!(counts(&on_state), (2, 2), "cache hits run no model");
}

/// HTTP against the local mock (reply `[1, 0]`): with `rerank_sync` on the
/// fresh response already carries the model's order — the fused top two
/// swapped, the rest in fused order.
#[tokio::test]
async fn http_rerank_sync_returns_model_order_in_fresh_response() {
    let (_, original) = make_test_app();
    let mut fused_config = original.config.clone();
    fused_config.search.decay.l0_lambda = 0.0;
    fused_config.search.decay.l1_lambda = 0.0;
    let (fused_app, fused_state) = make_test_app_cfg(fused_config);
    seed(&fused_state).await;
    let fused = http(&fused_app, "lantern").await;
    let fused = ids(&fused);
    assert_eq!(fused.len(), 3);

    let (base_url, _requests) = fake_rerank_provider().await;
    let mut config = local_rerank_config(&base_url);
    config.search.decay.l0_lambda = 0.0;
    config.search.decay.l1_lambda = 0.0;
    config.search.rerank_sync = true;
    let (app, state) = make_test_app_cfg(config);
    #[cfg(feature = "onnx-rerank")]
    assert!(state.onnx_reranker.is_none());
    seed(&state).await;

    let response = http(&app, "lantern").await;
    assert_eq!(ids(&response), [fused[1], fused[0], fused[2]]);
    assert_eq!(counts(&state), (1, 1));
}
