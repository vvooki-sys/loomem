use super::*;
use crate::tests::make_test_app;

fn request(policy: OperationPolicy, reference: Option<&str>) -> SearchRequest {
    serde_json::from_value(serde_json::json!({
        "query": "What happened last month?", "operation_policy": policy,
        "reference_time": reference, "top_k": 10
    }))
    .unwrap()
}

#[test]
fn e6_reference_normalization_binds_only_semantic_cache() {
    let (_app, state) = make_test_app();
    let mut config = state.config.search.clone();
    config.operation.enabled = true;
    let identity = |policy, reference| {
        let payload = request(policy, reference);
        let operation = prepare(&config, &payload, 10, None).unwrap();
        assert_eq!(operation.plan, OperationPlan::plain());
        assert!(operation.e2_lane_spec().is_none());
        operation.cache_identity(&resolve_date(&payload, &operation))
    };
    assert!(identity(OperationPolicy::E6, None)
        .unwrap()
        .starts_with("e6-semantic-evidence-v2-temporal-veto-v1|"));
    let utc = Some("2026-10-19T10:00:00Z");
    let offset = Some("2026-10-19T12:00:00+02:00");
    assert_eq!(
        identity(OperationPolicy::E6, utc),
        identity(OperationPolicy::E6, offset)
    );
    assert_ne!(
        identity(OperationPolicy::E6, utc),
        identity(OperationPolicy::E6, None)
    );
    assert_eq!(
        identity(OperationPolicy::E6, Some("invalid")),
        identity(OperationPolicy::E6, None)
    );
    for policy in [
        OperationPolicy::E0,
        OperationPolicy::E4,
        OperationPolicy::E5,
    ] {
        assert_eq!(identity(policy, utc), identity(policy, None));
        assert_ne!(identity(policy, utc), identity(OperationPolicy::E6, utc));
    }
}

#[test]
fn e6_obeys_common_operation_gate() {
    let (_app, state) = make_test_app();
    let payload = request(OperationPolicy::E6, None);
    let prepared = prepare(&state.config.search, &payload, 10, None).unwrap();
    assert_eq!(prepared.policy, OperationPolicy::E0);
    assert_eq!(
        prepared.cache_identity(&resolve_date(&payload, &prepared)),
        None
    );
}

pub(super) async fn http_results(app: &axum::Router, policy: OperationPolicy) -> serde_json::Value {
    http_scoped_results(app, policy, &["streamA"]).await
}

pub(super) async fn http_scoped_results(
    app: &axum::Router,
    policy: OperationPolicy,
    streams: &[&str],
) -> serde_json::Value {
    http_query_results(app, policy, streams, "apple budget").await
}

pub(super) async fn http_query_results(
    app: &axum::Router,
    policy: OperationPolicy,
    streams: &[&str],
    query: &str,
) -> serde_json::Value {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/search")
        .header("Authorization", "Bearer admin-token")
        .header("Content-Type", "application/json")
        .body(axum::body::Body::from(
            serde_json::to_vec(&serde_json::json!({
                "query": query, "operation_policy": policy, "top_k": 10,
                "streams": streams, "source_agent": "tester"
            }))
            .unwrap(),
        ))
        .unwrap();
    let (status, bytes) = crate::tests::send(app.clone(), request).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let response: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    response["results"].clone()
}

#[tokio::test]
async fn e6_missing_key_http_preserves_e0_results_and_isolates_cache() {
    let (_, original) = make_test_app();
    let mut config = original.config.clone();
    config.search.operation.enabled = true;
    config.llm.api_key = None;
    config.llm.api_key_env = "LOOMEM_E6_TEST_INTENTIONALLY_ABSENT_KEY".into();
    // This fallback/cache test compares all fields; time decay is not under test.
    config.search.decay.l0_lambda = 0.0;
    config.search.decay.l1_lambda = 0.0;
    let (app, state) = crate::tests::make_test_app_cfg(config);
    super::super::stream_scoping_e2e_tests::seed_one(
        &state,
        "eligible",
        "streamA",
        "apple budget has useful evidence",
        None,
    )
    .await;
    // Saturate access importance so repeated HTTP calls cannot mutate fixture scores.
    let mut chunk = state.store.get_chunk("eligible").unwrap().unwrap();
    chunk.importance = Some(1.5);
    state.store.store_chunk(&chunk).unwrap();
    let baseline = http_results(&app, OperationPolicy::E0).await;
    assert!(!baseline.as_array().unwrap().is_empty());
    assert_eq!(baseline, http_results(&app, OperationPolicy::E6).await);
    assert_eq!(state.query_cache.lock().await.len(), 2);
    let warm = http_results(&app, OperationPolicy::E6).await;
    assert_eq!(warm, http_results(&app, OperationPolicy::E0).await);
    assert_eq!(baseline[0]["content"], warm[0]["content"]);
    assert_eq!(baseline[0]["id"], warm[0]["id"]);
}
