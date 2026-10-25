use super::*;
use crate::tests::{make_test_app, make_test_app_cfg, send};
use axum::{body::Body, http::Request};
use serde_json::{json, Value};

async fn request(app: &axum::Router, policy: &str, trace: bool) -> Value {
    let request = Request::builder()
        .method("POST")
        .uri("/v1/search")
        .header("Authorization", "Bearer admin-token")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "query":"apple", "operation_policy":policy, "top_k":10,
                "streams":["streamA"], "source_agent":"tester", "trace":trace,
                "dry_run":true
            }))
            .unwrap(),
        ))
        .unwrap();
    let (status, bytes) = send(app.clone(), request).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    serde_json::from_slice(&bytes).unwrap()
}

fn ids(value: &Value) -> Vec<String> {
    value["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_string())
        .collect()
}

fn stage(value: &Value, name: &str) -> Vec<String> {
    value["trace_metadata"]["evidence"]["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["stage"] == name)
        .unwrap()["chunk_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn http_response_trace_matches_actual_e0_e2_e4_e6_and_untraced_order() {
    let (_, original) = make_test_app();
    let mut config = original.config.clone();
    config.search.operation.enabled = true;
    config.search.decay.l0_lambda = 0.0;
    config.search.decay.l1_lambda = 0.0;
    config.llm.api_key = None;
    config.llm.api_key_env = "LOOMEM_TRACE_TEST_ABSENT_KEY".into();
    let (app, state) = make_test_app_cfg(config);
    for (id, stream, content) in [
        ("a1", "streamA", "apple orchard has green trees beside the northern field and quiet river"),
        ("a2", "streamA", "river apple orchard has green trees beside the northern field and quiet river"),
        ("a3", "streamA", "apple cider production requires crushing fruit pressing juice and careful fermentation with clean equipment correct storage suitable temperature and patient observation before bottling the finished drink for later serving"),
        ("outside", "streamB", "apple orchard"),
    ] {
        super::super::stream_scoping_e2e_tests::seed_one(&state, id, stream, content, None).await;
    }
    for policy in ["E0", "E2-D", "E4", "E6"] {
        let traced = request(&app, policy, true).await;
        let returned = ids(&traced);
        assert!(!returned.is_empty(), "{policy}");
        assert_eq!(stage(&traced, "response"), returned, "{policy}");
        assert_eq!(
            ids(&request(&app, policy, false).await),
            returned,
            "{policy}"
        );
        let stages = traced["trace_metadata"]["evidence"]["stages"]
            .as_array()
            .unwrap();
        assert_eq!(stages.len(), 4);
        assert_eq!(
            stages
                .iter()
                .filter(|row| row["stage"] == "response")
                .count(),
            1
        );
        assert!(!traced["trace_metadata"].to_string().contains("outside"));
        for rejection in traced["trace_metadata"]["rejections"].as_array().unwrap() {
            assert!(!returned.contains(&rejection["chunk_id"].as_str().unwrap().to_string()));
        }
        if policy == "E4" {
            assert_ne!(
                stage(&traced, "after_filters_and_dedup"),
                returned,
                "must exercise actual final reorder"
            );
        }
    }
}

#[test]
fn restored_ids_have_no_terminal_rejection_and_displacements_name_the_lane() {
    let (_, state) = make_test_app();
    let payload: SearchRequest = serde_json::from_value(json!({
        "query":"q", "trace":true, "top_k":1, "stream":"test", "dry_run":true
    }))
    .unwrap();
    let auth = crate::auth::AuthContext::single_stream(
        "test",
        loomem_core::storage::UserRole::Admin,
        crate::auth::KeyScope::Shared,
        None,
        true,
    );
    let ctx = super::super::prepare_query_context(&state, &auth, &payload).unwrap();
    let first = super::tests::seed(&state, "first", "test");
    let restored = super::tests::seed(&state, "restored", "test");
    let (_, mut trace) = filter_with_trace(vec![first, restored.clone()], &ctx, &payload, &state);
    capture(
        &mut trace,
        "after_operation",
        std::slice::from_ref(&restored),
        &ctx,
        &state,
    );
    reconcile(&mut trace, &[restored], &ctx, &state);
    let rejections = trace.unwrap().rejections;
    assert_eq!(rejections.len(), 1);
    assert_eq!(rejections[0].chunk_id, "first");
    assert_eq!(rejections[0].stage, "response");
    assert_eq!(rejections[0].reason, RejectionReason::OperationFinalization);
}

#[tokio::test]
async fn final_authoritative_date_pass_is_observed_after_lane_finalization() {
    let (_, state) = make_test_app();
    let payload: SearchRequest = serde_json::from_value(json!({
        "query":"q", "trace":true, "top_k":10, "stream":"test", "dry_run":true
    }))
    .unwrap();
    let auth = crate::auth::AuthContext::single_stream(
        "test",
        loomem_core::storage::UserRole::Admin,
        crate::auth::KeyScope::Shared,
        None,
        true,
    );
    let mut ctx = super::super::prepare_query_context(&state, &auth, &payload).unwrap();
    let eligible = super::tests::seed(&state, "eligible", "test");
    let stale = super::tests::seed(&state, "stale", "test");
    let (outcome, trace) = filter_with_trace(vec![eligible, stale], &ctx, &payload, &state);
    let metrics = super::super::telemetry::ResponseMetrics::full(
        &ctx,
        &outcome,
        trace,
        super::super::operation::OperationMetrics::default(),
    );
    let lane =
        super::super::operation_lane::finalize(outcome.results, 0, &ctx, &payload, &state).await;
    // Model an authoritative store update after ranking/primary filtering.
    // An operation lane's freshly added stale row must be observed identically.
    let mut chunk = state.store.get_chunk("stale").unwrap().unwrap();
    chunk.metadata = Some(json!({"_loomem_ingest":{"version":1,"kind":"source"}}));
    chunk.provenance_role = loomem_core::storage::ProvenanceRole::Evidence;
    chunk.source = Some(loomem_core::source_tag::SourceTag::from_agent(
        "mcp-ingest-source",
    ));
    chunk.persistent = true;
    chunk.consolidated = true;
    chunk.trust_level = Some("a2".into());
    assert!(loomem_core::storage::is_ingest_source(&chunk));
    state.store.store_chunk(&chunk).unwrap();
    ctx.date_filter = Some(super::super::DateFilter::Range(
        1_700_000_000,
        1_800_000_000,
    ));
    let (results, metrics) = finish(lane, metrics, &ctx, &state);
    assert_eq!(
        results
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        ["eligible"]
    );
    let trace = metrics.evidence.unwrap();
    assert_eq!(trace.stages.len(), 4);
    assert_eq!(trace.stages.last().unwrap().chunk_ids, ["eligible"]);
    assert_eq!(trace.rejections.len(), 1);
    assert_eq!(trace.rejections[0].chunk_id, "stale");
    assert_eq!(trace.rejections[0].stage, "response");
    assert_eq!(
        trace.rejections[0].reason,
        RejectionReason::EventDateOutOfRange
    );
}

#[tokio::test]
async fn real_e2_pipeline_restores_truncated_id_and_records_displaced_primary() {
    let state = super::super::stream_scoping_e2e_tests::configured_state(true);
    let payload: SearchRequest = serde_json::from_value(json!({
        "query":"Recommend a guitar", "operation_policy":"E2-F", "trace":true,
        "top_k":21, "stream":"test", "dry_run":true
    }))
    .unwrap();
    let auth = crate::auth::AuthContext::single_stream(
        "test",
        loomem_core::storage::UserRole::Admin,
        crate::auth::KeyScope::Shared,
        None,
        true,
    );
    let ctx = super::super::prepare_query_context(&state, &auth, &payload).unwrap();
    let query = ctx.operation.e2_lane_spec().unwrap().query.clone();
    super::super::stream_scoping_e2e_tests::seed_one(&state, "restored", "test", &query, None)
        .await;
    let mut candidates: Vec<_> = (0..21)
        .map(|i| super::tests::seed(&state, &format!("primary-{i:02}"), "test"))
        .collect();
    let mut restored = candidates[0].clone();
    restored.id = "restored".into();
    restored.content = query;
    candidates.push(restored);
    let (outcome, trace) = filter_with_trace(candidates, &ctx, &payload, &state);
    assert_eq!(outcome.results.len(), 22);
    assert_eq!(outcome.final_top_k, 21);
    let metrics = super::super::telemetry::ResponseMetrics::full(
        &ctx,
        &outcome,
        trace,
        super::super::operation::OperationMetrics::default(),
    );
    let mut primary = outcome.results;
    primary.truncate(21);
    let lane = super::super::operation_lane::finalize(primary, 0, &ctx, &payload, &state).await;
    let (results, metrics) = finish(lane, metrics, &ctx, &state);
    let ids: Vec<_> = results.iter().map(|row| row.id.clone()).collect();
    assert_eq!(ids.len(), 21);
    assert_eq!(
        ids[20], "restored",
        "actual lexical lane must restore truncated candidate"
    );
    let trace = metrics.evidence.unwrap();
    assert_eq!(trace.stages.len(), 4);
    assert_eq!(trace.stages[3].chunk_ids, ids);
    assert_eq!(trace.rejections.len(), 1);
    assert_eq!(trace.rejections[0].chunk_id, "primary-20");
    assert_eq!(
        trace.rejections[0].reason,
        RejectionReason::OperationFinalization
    );
}
