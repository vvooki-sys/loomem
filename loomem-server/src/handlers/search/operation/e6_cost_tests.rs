//! Actual HTTP route coverage for native E6 usage accounting.
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use axum::{extract::State, routing::post, Json, Router};
use loomem_core::cost_tracker::DailyCost;
use serde_json::{json, Value};

use super::*;
use crate::{
    tests::{make_test_app, make_test_app_cfg},
    AppState,
};

#[derive(Clone)]
struct Provider {
    calls: Arc<AtomicUsize>,
    response: Arc<tokio::sync::Mutex<Value>>,
    status: Arc<tokio::sync::Mutex<axum::http::StatusCode>>,
}

async fn complete(
    State(provider): State<Provider>,
    Json(body): Json<Value>,
) -> (axum::http::StatusCode, Json<Value>) {
    assert_eq!(body["model"], "gpt-4.1");
    assert_eq!(body["service_tier"], "default");
    provider.calls.fetch_add(1, Ordering::SeqCst);
    (
        *provider.status.lock().await,
        Json(provider.response.lock().await.clone()),
    )
}

struct EndpointGuard(Option<String>);
impl Drop for EndpointGuard {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => std::env::set_var("LOOMEM_EVALUATION_SELECTOR_URL", value),
            None => std::env::remove_var("LOOMEM_EVALUATION_SELECTOR_URL"),
        }
    }
}

fn completion(content: &str, usage: bool) -> Value {
    let mut response = json!({"model":"gpt-4.1","service_tier":"default","choices":[{"finish_reason":"stop","message":{"content":content}}]});
    if usage {
        response["usage"] =
            json!({"prompt_tokens":1000,"completion_tokens":100,"total_tokens":1100});
    }
    response
}

fn daily(state: &AppState, stream: Option<&str>) -> DailyCost {
    let date = DailyCost::default().date;
    let key = match stream {
        Some(stream) => format!("cost:{stream}:{date}"),
        None => format!("cost:{date}"),
    };
    let cf = state.store.db().cf_handle("costs").unwrap();
    state
        .store
        .db()
        .get_cf(&cf, key.as_bytes())
        .unwrap()
        .map(|bytes| serde_json::from_slice(&bytes).unwrap())
        .unwrap_or_default()
}

fn seed_cap(state: &AppState, stream: Option<&str>) {
    let cost = DailyCost {
        total_usd: state.config.cost.daily_cap_usd,
        ..DailyCost::default()
    };
    let key = match stream {
        Some(stream) => format!("cost:{stream}:{}", cost.date),
        None => format!("cost:{}", cost.date),
    };
    let cf = state.store.db().cf_handle("costs").unwrap();
    state
        .store
        .db()
        .put_cf(&cf, key.as_bytes(), serde_json::to_vec(&cost).unwrap())
        .unwrap();
}

async fn fixture(model: &str) -> (Router, Arc<AppState>) {
    fixture_cap(model, 10.0).await
}

async fn fixture_cap(model: &str, cap: f64) -> (Router, Arc<AppState>) {
    fixture_settings(model, cap, true).await
}

async fn fixture_settings(model: &str, cap: f64, persist: bool) -> (Router, Arc<AppState>) {
    let (_, original) = make_test_app();
    let mut config = original.config.clone();
    config.search.operation.enabled = true;
    // Identity assertions cover complete results without wall-clock decay variance.
    config.search.decay.l0_lambda = 0.0;
    config.search.decay.l1_lambda = 0.0;
    config.llm.api_key = Some("synthetic-selector-test-key".into());
    config.llm.compression_model = model.into();
    config.cost.persist = persist;
    config.cost.daily_cap_usd = cap;
    let (app, state) = make_test_app_cfg(config);
    super::super::stream_scoping_e2e_tests::seed_one(
        &state,
        "eligible",
        "streamA",
        "apple budget is a synthetic example",
        None,
    )
    .await;
    let mut chunk = state.store.get_chunk("eligible").unwrap().unwrap();
    chunk.importance = Some(1.5);
    state.store.store_chunk(&chunk).unwrap();
    (app, state)
}

#[tokio::test]
async fn e6_native_http_books_paid_invalid_and_enforces_both_caps() {
    let provider = Provider {
        calls: Arc::new(AtomicUsize::new(0)),
        status: Arc::new(tokio::sync::Mutex::new(axum::http::StatusCode::OK)),
        response: Arc::new(tokio::sync::Mutex::new(completion(
            r#"{"selections":[]}"#,
            true,
        ))),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let _guard = EndpointGuard(std::env::var("LOOMEM_EVALUATION_SELECTOR_URL").ok());
    std::env::set_var("LOOMEM_EVALUATION_SELECTOR_URL", endpoint);
    let mock = Router::new()
        .route("/v1/chat/completions", post(complete))
        .with_state(provider.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, mock).await.unwrap();
    });
    verify_temporal_veto(&provider).await;
    let reservation = verify_paid_responses(&provider).await;
    verify_unpriced_tiers(&provider, reservation).await;
    verify_concurrent_unknown(&provider, reservation).await;
    verify_blocked_calls(&provider).await;
    server.abort();
}

async fn verify_paid_responses(provider: &Provider) -> f64 {
    let mut reservation = 0.0;
    for (content, known_usage, status) in [
        (r#"{"selections":[]}"#, true, axum::http::StatusCode::OK),
        ("invalid json", true, axum::http::StatusCode::OK),
        ("invalid json", true, axum::http::StatusCode::BAD_REQUEST),
        ("invalid json", false, axum::http::StatusCode::OK),
    ] {
        *provider.status.lock().await = status;
        *provider.response.lock().await = completion(content, known_usage);
        let (app, state) = fixture("gpt-4.1").await;
        let before = provider.calls.load(Ordering::SeqCst);
        let result = super::e6_tests::http_results(&app, OperationPolicy::E6).await;
        assert_eq!(result.as_array().unwrap().len(), 1);
        assert_eq!(provider.calls.load(Ordering::SeqCst), before + 1);
        let global = daily(&state, None);
        let stream = daily(&state, Some("streamA"));
        assert!(global.total_usd > 0.0);
        assert_eq!(global.total_usd, stream.total_usd);
        if known_usage {
            assert!((global.total_usd - 0.0028).abs() < 1e-12);
            assert_eq!(global.total_input_tokens, 1000);
            assert_eq!(global.total_output_tokens, 100);
            assert_eq!(global.selector_unknown_usage_calls, 0);
            assert_eq!(global.selector_reserved_usd, 0.0);
        } else {
            reservation = global.total_usd;
            assert_eq!(global.selector_unknown_usage_calls, 1);
            assert_eq!(global.selector_reserved_usd, global.total_usd);
        }
        assert_eq!(daily(&state, Some("outside")).total_usd, 0.0);
        super::e6_tests::http_results(&app, OperationPolicy::E6).await;
        assert_eq!(provider.calls.load(Ordering::SeqCst), before + 1);
        assert_eq!(daily(&state, None).total_usd, global.total_usd);
    }
    reservation
}

async fn verify_unpriced_tiers(provider: &Provider, reservation: f64) {
    for tier in [None, Some("priority"), Some("flex")] {
        let mut response = completion(r#"{"selections":[]}"#, true);
        match tier {
            Some(tier) => response["service_tier"] = json!(tier),
            None => {
                response.as_object_mut().unwrap().remove("service_tier");
            }
        }
        *provider.response.lock().await = response;
        let (app, state) = fixture("gpt-4.1").await;
        let before = provider.calls.load(Ordering::SeqCst);
        super::e6_tests::http_results(&app, OperationPolicy::E6).await;
        assert_eq!(provider.calls.load(Ordering::SeqCst), before + 1);
        let cost = daily(&state, None);
        assert_eq!(cost.total_usd, reservation);
        assert_eq!(cost.selector_reserved_usd, reservation);
        assert_eq!(cost.selector_unknown_usage_calls, 1);
        assert_eq!(cost.total_input_tokens, 0);
        assert_eq!(cost.total_output_tokens, 0);
        assert_eq!(daily(&state, Some("streamA")).total_usd, reservation);
    }
}

async fn verify_concurrent_unknown(provider: &Provider, reservation: f64) {
    *provider.response.lock().await = completion("invalid json", false);
    let (app, state) = fixture_cap("gpt-4.1", reservation * 1.5).await;
    let before = provider.calls.load(Ordering::SeqCst);
    let (first, second) = tokio::join!(
        super::e6_tests::http_results(&app, OperationPolicy::E6),
        super::e6_tests::http_results(&app, OperationPolicy::E6),
    );
    assert_eq!(first[0]["content"], second[0]["content"]);
    assert_eq!(provider.calls.load(Ordering::SeqCst), before + 1);
    let cost = daily(&state, None);
    assert_eq!(cost.selector_calls, 1);
    assert_eq!(cost.selector_unknown_usage_calls, 1);
    assert!(cost.total_usd <= state.config.cost.daily_cap_usd);
    assert_eq!(cost.total_usd, daily(&state, Some("streamA")).total_usd);
    state.query_cache.lock().await.clear();
    super::e6_tests::http_results(&app, OperationPolicy::E6).await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), before + 1);
}

async fn verify_blocked_calls(provider: &Provider) {
    for scope in [None, Some("streamA")] {
        let (app, state) = fixture("gpt-4.1").await;
        seed_cap(&state, scope);
        let before = provider.calls.load(Ordering::SeqCst);
        let result = super::e6_tests::http_results(&app, OperationPolicy::E6).await;
        assert_eq!(result[0]["content"], "apple budget is a synthetic example");
        assert_eq!(provider.calls.load(Ordering::SeqCst), before);
    }
    let (app, state) = fixture("gpt-4.1").await;
    let before = provider.calls.load(Ordering::SeqCst);
    let scope = &["streamA", "outside"];
    let baseline = super::e6_tests::http_scoped_results(&app, OperationPolicy::E0, scope).await;
    let fallback = super::e6_tests::http_scoped_results(&app, OperationPolicy::E6, scope).await;
    // The common gate shares the E0 cache namespace for this unsupported scope.
    assert_eq!(baseline[0]["content"], fallback[0]["content"]);
    assert_eq!(baseline[0]["id"], fallback[0]["id"]);
    assert_eq!(provider.calls.load(Ordering::SeqCst), before);
    assert_eq!(daily(&state, None).total_usd, 0.0);

    let (app, state) = fixture_settings("gpt-4.1", 10.0, false).await;
    let before = provider.calls.load(Ordering::SeqCst);
    super::e6_tests::http_results(&app, OperationPolicy::E6).await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), before);
    assert_eq!(daily(&state, None).total_usd, 0.0);

    let (app, state) = fixture("unpriced-synthetic-model").await;
    let before = provider.calls.load(Ordering::SeqCst);
    super::e6_tests::http_results(&app, OperationPolicy::E6).await;
    assert_eq!(provider.calls.load(Ordering::SeqCst), before);
    assert_eq!(daily(&state, None).total_usd, 0.0);
}

fn cost_rows(state: &AppState) -> Vec<(Vec<u8>, Vec<u8>)> {
    let cf = state.store.db().cf_handle("costs").unwrap();
    let mut iterator = state.store.db().raw_iterator_cf(&cf);
    iterator.seek_to_first();
    let mut rows = Vec::new();
    while iterator.valid() {
        rows.push((
            iterator.key().unwrap().to_vec(),
            iterator.value().unwrap().to_vec(),
        ));
        iterator.next();
    }
    iterator.status().unwrap();
    rows
}

async fn verify_temporal_veto(provider: &Provider) {
    let (app, state) = fixture("gpt-4.1").await;
    super::super::stream_scoping_e2e_tests::seed_one(
        &state,
        "second",
        "streamA",
        "apple budget has a separate synthetic note",
        None,
    )
    .await;
    let mut chunk = state.store.get_chunk("second").unwrap().unwrap();
    chunk.importance = Some(1.5);
    state.store.store_chunk(&chunk).unwrap();
    let query = "What is my current apple budget?";
    let baseline =
        super::e6_tests::http_query_results(&app, OperationPolicy::E0, &["streamA"], query).await;
    assert_eq!(baseline.as_array().unwrap().len(), 2);
    let before = provider.calls.load(Ordering::SeqCst);
    let costs = cost_rows(&state);
    let vetoed =
        super::e6_tests::http_query_results(&app, OperationPolicy::E6, &["streamA"], query).await;
    assert_eq!(vetoed, baseline);
    assert_eq!(provider.calls.load(Ordering::SeqCst), before);
    assert_eq!(cost_rows(&state), costs);
    let warm_e0 =
        super::e6_tests::http_query_results(&app, OperationPolicy::E0, &["streamA"], query).await;
    let warm_e6 =
        super::e6_tests::http_query_results(&app, OperationPolicy::E6, &["streamA"], query).await;
    assert_eq!(warm_e6, warm_e0);
    assert_eq!(provider.calls.load(Ordering::SeqCst), before);
    assert_eq!(cost_rows(&state), costs);
}
