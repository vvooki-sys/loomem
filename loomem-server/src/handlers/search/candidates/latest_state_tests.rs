//! `[search] latest_state_boost` end to end through `/v1/search`, on the
//! BM25-only path (a two-word query is `Simple`, so no vector channel): the
//! older chunk matches the query better, the newer one wins only when the
//! boost is on and the query asks about the current state.
use crate::tests::{make_test_app, make_test_app_cfg, send};
use crate::AppState;
use axum::{body::Body, http::Request};
use loomem_core::storage::Chunk;
use loomem_core::tantivy_index::TextDocument;
use serde_json::{json, Value};
use std::sync::Arc;

/// 2024-01-01T00:00:00Z and 2025-01-01T00:00:00Z.
const OLD: u64 = 1_704_067_200;
const NEW: u64 = 1_735_689_600;

fn chunk(id: &str, content: &str, timestamp: u64) -> Chunk {
    serde_json::from_value(json!({
        "id":id,"content":content,"stream":"streamA","level":0,"score":1.0,
        "timestamp":timestamp,"consolidated":false,"dormant":false,"in_progress":false
    }))
    .unwrap()
}

async fn seed(state: &Arc<AppState>, chunk: &Chunk) {
    state.store.store_chunk(chunk).unwrap();
    let mut index = state.tantivy.lock().await;
    index
        .upsert_document(TextDocument {
            id: chunk.id.clone(),
            content: chunk.content.clone(),
            user_id: "default".into(),
            app_id: "default".into(),
            level: 0,
            timestamp: i64::try_from(chunk.timestamp).unwrap(),
            stream: chunk.stream.clone(),
            entities: None,
            relations: None,
            event_date: None,
            source_agent: None,
        })
        .unwrap();
    index.commit().unwrap();
}

/// Time decay off: it reads the wall clock and would otherwise favour the
/// newer chunk on its own.
async fn app(boost: f64) -> axum::Router {
    let (_, original) = make_test_app();
    let mut config = original.config.clone();
    config.search.decay.l0_lambda = 0.0;
    config.search.decay.l1_lambda = 0.0;
    config.search.latest_state_boost = boost;
    let (app, state) = make_test_app_cfg(config);
    seed(&state, &chunk("stale", "orchard orchard pears", OLD)).await;
    seed(&state, &chunk("update", "orchard apples", NEW)).await;
    app
}

async fn ranked(app: &axum::Router, query: &str) -> Vec<(String, f64)> {
    ranked_payload(app, json!({"query":query,"streams":["streamA"],"top_k":10})).await
}

async fn ranked_payload(app: &axum::Router, payload: Value) -> Vec<(String, f64)> {
    let request = Request::builder()
        .method("POST")
        .uri("/v1/search")
        .header("Authorization", "Bearer admin-token")
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();
    let (status, body) = send(app.clone(), request).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let value: Value = serde_json::from_slice(&body).unwrap();
    value["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            let id = row["id"].as_str().unwrap().to_string();
            (id, row["score"].as_f64().unwrap())
        })
        .collect()
}

fn ids(rows: &[(String, f64)]) -> Vec<&str> {
    rows.iter().map(|(id, _)| id.as_str()).collect()
}

#[tokio::test]
async fn http_boost_off_keeps_the_better_lexical_match_first() {
    let off = ranked(&app(1.0).await, "orchard now").await;
    assert_eq!(ids(&off), ["stale", "update"]);
}

#[tokio::test]
async fn http_boost_on_ranks_the_newer_chunk_first_for_a_current_state_query() {
    let off = ranked(&app(1.0).await, "orchard now").await;
    let on = ranked(&app(2.0).await, "orchard now").await;
    assert_eq!(ids(&on), ["update", "stale"]);
    // Two distinct times: the newer chunk is doubled, the older kept as is.
    let score = |rows: &[(String, f64)], id: &str| {
        rows.iter()
            .find(|(hit, _)| hit == id)
            .map(|(_, s)| *s)
            .unwrap()
    };
    assert_eq!(score(&on, "stale"), score(&off, "stale"));
    assert!((score(&on, "update") - 2.0 * score(&off, "update")).abs() < 1e-9);
}

#[tokio::test]
async fn http_boost_on_without_a_marker_is_identical() {
    let off = ranked(&app(1.0).await, "orchard pears").await;
    let on = ranked(&app(2.0).await, "orchard pears").await;
    assert_eq!(on, off);
}

/// An explicit date range (here covering both chunks) turns the promotion
/// off: an "as of <date>" question must not be pushed toward newer facts.
#[tokio::test]
async fn http_boost_on_with_a_date_filter_is_identical() {
    let payload = json!({"query":"orchard now","streams":["streamA"],"top_k":10,
        "date_from":"2023-06-01","date_to":"2025-06-01"});
    let off = ranked_payload(&app(1.0).await, payload.clone()).await;
    let on = ranked_payload(&app(2.0).await, payload).await;
    assert_eq!(ids(&off), ["stale", "update"]);
    assert_eq!(on, off);
}
