use super::super::stream_scoping_e2e_tests::seed_one;
use super::*;
use crate::tests::{make_test_app, make_test_app_cfg, send};
use axum::{body::Body, http::Request};

fn payload(policy: OperationPolicy) -> SearchRequest {
    serde_json::from_value(serde_json::json!({
        "query": "apple", "operation_policy": policy, "top_k": 10,
        "streams": ["streamA"], "source_agent": "tester"
    }))
    .unwrap()
}

#[test]
fn e4_is_plain_but_has_its_own_cache_identity_and_common_gates() {
    let (_app, state) = make_test_app();
    let mut config = state.config.search.clone();
    config.operation.enabled = true;
    let request = payload(OperationPolicy::E4);
    let prepared = prepare(&config, &request, 10, None).unwrap();
    assert_eq!(prepared.policy, OperationPolicy::E4);
    assert_eq!(prepared.plan, OperationPlan::plain());
    assert!(!prepared.uses_operation_primary_merge());
    assert!(prepared.e2_lane_spec().is_none());
    let date = resolve_date(&request, &prepared);
    assert_eq!(
        prepared.cache_identity(&date).as_deref(),
        Some("e4-context-selection-v1")
    );
    for reason in ["disabled", "rerank", "multi", "rare", "limit", "streams"] {
        let mut gated = config.clone();
        match reason {
            "disabled" => gated.operation.enabled = false,
            "rerank" => gated.rerank_enabled = true,
            "multi" => gated.multi_query_enabled = true,
            "rare" => gated.rare_term_lane.enabled = true,
            "limit" => gated.top_k = 61,
            _ => {}
        }
        let streams = vec!["a".into(), "b".into()];
        let scope = (reason == "streams").then_some(streams.as_slice());
        let prepared = prepare(&gated, &request, 10, scope).unwrap();
        assert_eq!(prepared.policy, OperationPolicy::E0, "{reason}");
        assert_eq!(
            prepared.cache_identity(&resolve_date(&request, &prepared)),
            None
        );
    }
}

async fn response(app: &axum::Router, policy: OperationPolicy) -> serde_json::Value {
    let request = Request::builder()
        .method("POST")
        .uri("/v1/search")
        .header("Authorization", "Bearer admin-token")
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&serde_json::json!({"query": "apple", "operation_policy": policy, "top_k": 10, "streams": ["streamA"], "source_agent": "tester"})).unwrap()))
        .unwrap();
    let (status, bytes) = send(app.clone(), request).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let mut result: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let records = result["results"].as_array_mut().unwrap();
    for record in records.iter_mut() {
        let object = record.as_object_mut().unwrap();
        object.remove("metadata");
        object.remove("score");
    }
    serde_json::json!({"results": records})
}

fn ids(response: &serde_json::Value) -> Vec<String> {
    response["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn e4_http_cache_isolated_warm_cold_permutation_and_scope() {
    let (_, original) = make_test_app();
    let mut config = original.config.clone();
    config.search.operation.enabled = true;
    let (app, state) = make_test_app_cfg(config);
    for (id, content) in [
        ("a1", "apple orchard has green trees beside the northern field and quiet river"),
        ("a2", "river apple orchard has green trees beside the northern field and quiet river"),
        ("a3", "apple cider production requires crushing fruit pressing juice and careful fermentation with clean equipment correct storage suitable temperature and patient observation before bottling the finished drink for later serving"),
    ] {
        seed_one(&state, id, "streamA", content, None).await;
    }
    seed_one(&state, "outside", "streamB", "apple orchard", None).await;
    let baseline = response(&app, OperationPolicy::E0).await;
    assert_eq!(state.query_cache.lock().await.len(), 1);
    let selected = response(&app, OperationPolicy::E4).await;
    assert_eq!(state.query_cache.lock().await.len(), 2);
    assert_eq!(selected, response(&app, OperationPolicy::E4).await);
    assert_eq!(baseline, response(&app, OperationPolicy::E0).await);
    let original_ids = ids(&baseline);
    let selected_ids = ids(&selected);
    assert_eq!(original_ids.len(), 3);
    assert_ne!(original_ids, selected_ids, "fixture must exercise deferral");
    assert_eq!(original_ids.first(), selected_ids.first());
    let mut original_set = original_ids;
    let mut selected_set = selected_ids;
    original_set.sort();
    selected_set.sort();
    assert_eq!(original_set, selected_set);
    assert!(!selected_set.iter().any(|id| id == "outside"));
    state.query_cache.lock().await.clear();
    assert_eq!(selected, response(&app, OperationPolicy::E4).await);
    assert_eq!(baseline, response(&app, OperationPolicy::E0).await);
}

#[tokio::test]
async fn disabled_e4_http_returns_and_caches_exact_e0() {
    let (app, state) = make_test_app();
    assert!(!state.config.search.operation.enabled);
    seed_one(&state, "eligible", "streamA", "apple orchard fruit", None).await;
    let baseline = response(&app, OperationPolicy::E0).await;
    assert_eq!(ids(&baseline), vec!["eligible"]);
    assert_eq!(baseline, response(&app, OperationPolicy::E4).await);
    assert_eq!(state.query_cache.lock().await.len(), 1);
    state.query_cache.lock().await.clear();
    assert_eq!(baseline, response(&app, OperationPolicy::E4).await);
    assert_eq!(baseline, response(&app, OperationPolicy::E0).await);
    assert_eq!(state.query_cache.lock().await.len(), 1);
}
