//! `[search] content_dedup` end to end through `/v1/search`: the default
//! envelope equals an explicit `prefix`, which hides an update that only
//! differs after the first 150 chars; `exact` returns both chunks.
use crate::tests::{make_test_app, make_test_app_cfg, send};
use axum::{body::Body, http::Request};
use loomem_core::search::ContentDedup;
use loomem_core::storage::Chunk;
use loomem_core::tantivy_index::TextDocument;
use serde_json::{json, Value};

const RECORDED: i64 = 1_704_067_200;
const HOTEL: &str = "For our autumn project planning conversation, I want to record my current \
                     travel preference for the upcoming conference and the accommodation arrangements. ";

fn chunk(id: &str, content: String) -> Chunk {
    serde_json::from_value(json!({
        "id":id,"content":content,"stream":"streamA",
        "level":0,"score":1.0,"timestamp":RECORDED,
        "consolidated":false,"dormant":false,"in_progress":false,"persistent":false
    }))
    .unwrap()
}

/// A keyless test app on the shipped defaults, or on `mode`, with the old
/// preference and its negated update indexed. Time decay is off: it reads
/// the wall clock in whole seconds.
async fn app(mode: Option<ContentDedup>) -> axum::Router {
    let mut config = make_test_app().1.config.clone();
    config.search.decay.l0_lambda = 0.0;
    config.search.decay.l1_lambda = 0.0;
    config.llm.api_key = None;
    config.llm.api_key_env = "LOOMEM_TEST_KEY_THAT_IS_NEVER_SET".into();
    config.llm.providers.clear();
    assert!(
        config.llm.active_provider().is_err(),
        "test must stay keyless"
    );
    if let Some(mode) = mode {
        config.search.content_dedup = mode;
    }
    let (app, state) = make_test_app_cfg(config);
    for item in [
        chunk("old", format!("{HOTEL}I want a lively hotel.")),
        chunk("updated", format!("{HOTEL}I do not want a lively hotel.")),
    ] {
        state.store.store_chunk(&item).unwrap();
        let mut index = state.tantivy.lock().await;
        index
            .upsert_document(TextDocument {
                id: item.id.clone(),
                content: item.content.clone(),
                user_id: "default".into(),
                app_id: "default".into(),
                level: 0,
                timestamp: RECORDED,
                stream: item.stream.clone(),
                entities: None,
                relations: None,
                event_date: None,
                source_agent: None,
            })
            .unwrap();
        index.commit().unwrap();
    }
    app
}

/// The response with `took_ms` zeroed and rows in id order (the two rows
/// may tie on score).
async fn search(app: &axum::Router) -> Value {
    let payload = json!({"query":"lively hotel","streams":["streamA"],"top_k":20});
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
    let rows = value["results"].as_array_mut().unwrap();
    rows.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
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

#[tokio::test]
async fn http_explicit_prefix_is_identical_to_the_shipped_default() {
    let expected = search(&app(None).await).await;
    let actual = search(&app(Some(ContentDedup::Prefix)).await).await;
    assert_eq!(actual, expected);
    // Today's behaviour: the update shares the first 150 chars and is hidden.
    assert_eq!(ids(&expected).len(), 1, "{expected}");
}

#[tokio::test]
async fn http_exact_keeps_the_negated_update() {
    let response = search(&app(Some(ContentDedup::Exact)).await).await;
    assert_eq!(ids(&response), ["old", "updated"]);
}
