use super::*;
use axum::{
    body::{to_bytes, Body},
    http::Request as HttpRequest,
    middleware,
};
use loomem_core::{fragments::TokenCounterKind, storage::Chunk, tantivy_index::TextDocument};
use tower::ServiceExt;

pub(super) fn fixture() -> Arc<AppState> {
    let (_, original) = crate::tests::make_test_app();
    let mut cfg = original.config.clone();
    cfg.scheduler.enabled = false;
    cfg.dream.enabled = false;
    cfg.knowledge_extraction.enabled = false;
    cfg.entity_extraction.enabled = false;
    cfg.memory_generator.enabled = false;
    cfg.contradiction.enabled = false;
    cfg.profile.enabled = false;
    cfg.content_type.enabled = false;
    cfg.associator.enabled = false;
    cfg.llm.embedding_provider = "local".into();
    cfg.llm.embedding_dim = 384;
    cfg.storage.vector_enabled = true;
    cfg.storage.tantivy.enabled = true;
    cfg.fragments.enabled = true;
    cfg.fragments.token_counter = TokenCounterKind::Approx;
    crate::tests::make_test_app_cfg(cfg).1
}
pub(super) fn app(state: Arc<AppState>, enabled: bool, token: Option<&str>) -> Router {
    let config = AuthConfig {
        admin_token: token.map(str::to_owned),
    };
    routes_enabled(&config, enabled)
        .layer(middleware::from_fn(
            move |mut request: axum::extract::Request, next: middleware::Next| {
                request.extensions_mut().insert(config.clone());
                auth::auth_middleware(request, next)
            },
        ))
        .with_state(state)
}
fn payload() -> Value {
    json!({"schema_version":1,"scope":"synthetic-1","stream":"evaluation",
        "sources":[{"source_id":"00000000-0000-4000-8000-000000000001","fragment_ids":["00000000-0000-4000-8000-000000000001:f0"],
        "content":"user: synthetic green parcel", "conversation_date":"2026-10-01"}]})
}
async fn call(app: Router, payload: String, token: Option<&str>) -> (StatusCode, Value) {
    let mut request = HttpRequest::builder()
        .method("POST")
        .uri("/v1/evaluation/readiness")
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .oneshot(request.body(Body::from(payload)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), MAX_BYTES * 2).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
#[tokio::test]
async fn authentication_opt_in_and_configuration_are_fail_closed() {
    let mut state = fixture();
    for (enabled, configured, sent, status) in [
        (
            false,
            Some("fixture"),
            Some("fixture"),
            StatusCode::NOT_FOUND,
        ),
        (true, None, None, StatusCode::NOT_FOUND),
        (true, Some("fixture"), None, StatusCode::UNAUTHORIZED),
        (
            true,
            Some("fixture"),
            Some("wrong"),
            StatusCode::UNAUTHORIZED,
        ),
    ] {
        assert_eq!(
            call(
                app(state.clone(), enabled, configured),
                payload().to_string(),
                sent
            )
            .await
            .0,
            status
        );
    }
    Arc::get_mut(&mut state).unwrap().config.scheduler.enabled = true;
    assert_eq!(
        call(
            app(state, true, Some("fixture")),
            payload().to_string(),
            Some("fixture")
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}
#[tokio::test]
async fn malformed_and_oversized_expectations_are_rejected() {
    let state = fixture();
    for bad in [
        json!({}),
        json!({"schema_version":1,"scope":"x","stream":"evaluation","sources":[]}),
        {
            let mut p = payload();
            p["stream"] = json!("foreign:invalid");
            p
        },
        {
            let mut p = payload();
            p["sources"][0]["fragment_ids"] = json!([]);
            p
        },
        {
            let mut p = payload();
            p["sources"][0]["fragment_ids"] = json!(vec!["child"; 1024]);
            p
        },
    ] {
        assert!(call(
            app(state.clone(), true, Some("fixture")),
            bad.to_string(),
            Some("fixture")
        )
        .await
        .0
        .is_client_error());
    }
    assert_eq!(
        call(
            app(state, true, Some("fixture")),
            " ".repeat(MAX_BYTES + 1),
            Some("fixture")
        )
        .await
        .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
}
async fn persist(state: &AppState) -> Vec<Chunk> {
    let source: SourceExpectation =
        serde_json::from_value(payload()["sources"][0].clone()).unwrap();
    let records = expected_bytes::reconstruct(state, &source).unwrap();
    let mut chunks = Vec::new();
    let mut index = state.tantivy.lock().await;
    for record in records {
        let kind = if record.fragment.is_some() {
            "fragment"
        } else {
            "source"
        };
        let mut chunk: Chunk =
            serde_json::from_value(json!({"id":record.id,"content":record.content,
            "stream":"evaluation","level":0,"score":1.0,"timestamp":1,
            "consolidated":false,"dormant":false,"in_progress":false}))
            .unwrap();
        chunk.metadata = Some(
            json!({"_loomem_ingest":{"version":1,"kind":kind,"conversation_date":"2026-10-01"}}),
        );
        chunk.source_ids = record
            .fragment
            .as_ref()
            .map(|meta| vec![meta.source_id.clone()]);
        chunk.fragment = record.fragment;
        state.store.store_chunk(&chunk).unwrap();
        state
            .store
            .store_embedding(&chunk.id, vec![0.1; 384])
            .unwrap();
        index
            .index_document(TextDocument {
                id: chunk.id.clone(),
                content: chunk.content.clone(),
                stream: chunk.stream.clone(),
                user_id: String::new(),
                app_id: String::new(),
                level: 0,
                timestamp: 1,
                entities: None,
                relations: None,
                event_date: None,
                source_agent: None,
            })
            .unwrap();
        chunks.push(chunk);
    }
    chunks
}
#[tokio::test]
async fn observes_exact_records_without_committing_or_repairing() {
    let state = fixture();
    let chunks = persist(&state).await;
    for _ in 0..2 {
        let (status, proof) = call(
            app(state.clone(), true, Some("fixture")),
            payload().to_string(),
            Some("fixture"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(proof["ready"], false);
        assert_eq!(proof["records"][0]["document"], "missing");
    }
    state.tantivy.lock().await.commit().unwrap();
    let (_, proof) = call(
        app(state.clone(), true, Some("fixture")),
        payload().to_string(),
        Some("fixture"),
    )
    .await;
    assert_eq!(proof["ready"], true);
    assert_eq!(proof["scope"], "synthetic-1");
    assert_eq!(proof["evidence"], EVIDENCE);
    assert_eq!(proof["records"].as_array().unwrap().len(), 2);
    state.store.delete_embedding(&chunks[0].id).unwrap();
    let (_, missing) = call(
        app(state.clone(), true, Some("fixture")),
        payload().to_string(),
        Some("fixture"),
    )
    .await;
    assert_eq!(missing["ready"], false);
    assert_eq!(missing["records"][0]["vector"], "missing");
    assert!(state.store.get_embedding(&chunks[0].id).unwrap().is_none());
    state
        .store
        .store_embedding(&chunks[0].id, vec![0.1; 384])
        .unwrap();
    let mut source = chunks[0].clone();
    source.metadata = None;
    state.store.store_chunk(&source).unwrap();
    let (_, proof) = call(
        app(state.clone(), true, Some("fixture")),
        payload().to_string(),
        Some("fixture"),
    )
    .await;
    assert_eq!(proof["ready"], false);
    assert_eq!(proof["records"][0]["chunk"], "metadata_mismatch");
    assert!(state
        .store
        .get_chunk("00000000-0000-4000-8000-000000000001")
        .unwrap()
        .unwrap()
        .metadata
        .is_none());
    let mut foreign = payload();
    foreign["stream"] = json!("another");
    let (_, proof) = call(
        app(state, true, Some("fixture")),
        foreign.to_string(),
        Some("fixture"),
    )
    .await;
    assert_eq!(proof["records"][0]["chunk"], "unavailable");
}

#[test]
fn protocol_record_limit_and_incompatible_configurations() {
    let mut request: Request = serde_json::from_value(payload()).unwrap();
    request.sources[0].fragment_ids = (0..MAX_RECORDS).map(|n| format!("fragment-{n}")).collect();
    assert_eq!(validate(&request), Err(StatusCode::BAD_REQUEST));
    request.sources[0].fragment_ids.pop();
    assert!(validate(&request).is_ok());
    let state = fixture();
    assert!(compatible(&state.config));
    let mut cfg = state.config.clone();
    cfg.server.host = "0.0.0.0".into();
    assert!(!compatible(&cfg));
    cfg = state.config.clone();
    cfg.llm.embedding_provider = "openai".into();
    assert!(!compatible(&cfg));
    cfg = state.config.clone();
    cfg.storage.tantivy.enabled = false;
    assert!(!compatible(&cfg));
}
