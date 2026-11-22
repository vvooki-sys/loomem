use super::*;
use axum::{
    body::{to_bytes, Body},
    http::Request as HttpRequest,
};
use loomem_core::{source_tag::SourceTag, storage::ProvenanceRole, tantivy_index::TextDocument};
use tower::ServiceExt;

const SOURCE: &str = "00000000-0000-4000-8000-000000000001";
const FACT: &str = "00000000-0000-4000-8000-000000000002";

fn fixture() -> Arc<AppState> {
    let mut state = super::super::tests::fixture();
    Arc::get_mut(&mut state)
        .unwrap()
        .config
        .knowledge_extraction
        .enabled = true;
    state
}

fn payload() -> Value {
    let golden: Value = serde_json::from_str(include_str!(
        "../../mcp/dispatcher/ingest/extraction_receipt/fixtures/complete.json"
    ))
    .unwrap();
    json!({"schema_version":1,"scope":"whole-ingest-fixture","stream":"evaluation",
        "source":{"content":"user: I prefer quiet accommodation.",
            "conversation_date":"2024-03-20","source_id":SOURCE,
            "fragment_ids":[format!("{SOURCE}:f0")]},
        "facts":golden["extraction_receipt"]["facts"]})
}

async fn call(state: Arc<AppState>, body: Value) -> (StatusCode, Value) {
    call_route(
        state,
        body.to_string(),
        "/v1/evaluation/ingest-readiness",
        true,
        Some("fixture"),
    )
    .await
}

async fn call_route(
    state: Arc<AppState>,
    body: String,
    path: &str,
    enabled: bool,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let app = super::super::tests::app(state, enabled, Some("fixture"));
    let mut request = HttpRequest::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), MAX_BYTES * 2).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn persist(state: &AppState, input: &Value) -> Vec<Chunk> {
    let source: SourceExpectation = serde_json::from_value(input["source"].clone()).unwrap();
    let records = expected_bytes::reconstruct(state, &source).unwrap();
    let mut chunks = Vec::new();
    for record in records {
        let kind = if record.fragment.is_some() {
            "fragment"
        } else {
            "source"
        };
        let mut chunk: Chunk = serde_json::from_value(json!({
            "id":record.id,"content":record.content,"stream":"evaluation",
            "level":0,"score":1.0,"timestamp":1792432800,"consolidated":true,
            "dormant":false,"in_progress":false,"persistent":true}))
        .unwrap();
        chunk.metadata = Some(json!({"_loomem_ingest":{"version":1,"kind":kind,
            "conversation_date":source.conversation_date}}));
        chunk.trust_level = Some("a2".into());
        chunk.source = Some(SourceTag::from_agent(if kind == "source" {
            "mcp-ingest-source"
        } else {
            "mcp-ingest"
        }));
        chunk.provenance_role = ProvenanceRole::Evidence;
        chunk.source_ids = record
            .fragment
            .as_ref()
            .map(|meta| vec![meta.source_id.clone()]);
        chunk.fragment = record.fragment;
        save(state, &chunk).await;
        chunks.push(chunk);
    }
    for fact in input["facts"].as_array().unwrap() {
        let mut value = fact["expected"].clone();
        value["id"] = fact["id"].clone();
        value["stream"] = json!("evaluation");
        let chunk: Chunk = serde_json::from_value(value).unwrap();
        save(state, &chunk).await;
        chunks.push(chunk);
    }
    chunks
}

async fn save(state: &AppState, chunk: &Chunk) {
    state.store.store_chunk(chunk).unwrap();
    state
        .store
        .store_embedding(&chunk.id, vec![0.1; 384])
        .unwrap();
    state
        .tantivy
        .lock()
        .await
        .index_document(TextDocument {
            id: chunk.id.clone(),
            content: chunk.content.clone(),
            stream: chunk.stream.clone(),
            user_id: String::new(),
            app_id: String::new(),
            level: chunk.level,
            timestamp: i64::try_from(chunk.timestamp).unwrap(),
            entities: None,
            relations: None,
            event_date: None,
            source_agent: None,
        })
        .unwrap();
}

#[tokio::test]
async fn union_is_observed_in_existing_snapshot_without_commit_or_repair() {
    let state = fixture();
    persist(&state, &payload()).await;
    for _ in 0..2 {
        let (status, proof) = call(state.clone(), payload()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(proof["ready"], false);
        assert!(proof["records"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["document"] == "missing"));
    }
    state.tantivy.lock().await.commit().unwrap();
    let (status, proof) = call(state.clone(), payload()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(proof["ready"], true);
    assert_eq!(proof["evidence"], EVIDENCE);
    assert_eq!(proof["records"].as_array().unwrap().len(), 3);
    assert_eq!(proof["records"][2]["id"], FACT);
    assert!(proof["records"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r.as_object().unwrap().len() == 4));
    let mut old = payload();
    old.as_object_mut().unwrap().remove("facts");
    old["sources"] = json!([old.as_object_mut().unwrap().remove("source").unwrap()]);
    assert_eq!(
        call_route(
            state,
            old.to_string(),
            "/v1/evaluation/readiness",
            true,
            Some("fixture")
        )
        .await
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn v2_projects_verified_fragments_only_when_ready_and_keeps_v1_unchanged() {
    let state = fixture();
    persist(&state, &payload()).await;
    let route = "/v1/evaluation/ingest-readiness-v2";
    let (_, unready) = call_route(
        state.clone(),
        payload().to_string(),
        route,
        true,
        Some("fixture"),
    )
    .await;
    assert_eq!(unready["ready"], false);
    assert_eq!(unready["fragment_projections"], Value::Null);
    state.tantivy.lock().await.commit().unwrap();
    let (status, proof) = call_route(
        state.clone(),
        payload().to_string(),
        route,
        true,
        Some("fixture"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(proof["schema_version"], 2);
    assert_eq!(proof["evidence"], "fresh_immutable_ingest_projections_v2");
    assert_eq!(proof["ready"], true);
    let projections = proof["fragment_projections"].as_array().unwrap();
    assert_eq!(projections.len(), 1);
    assert_eq!(projections[0]["id"], format!("{SOURCE}:f0"));
    assert_eq!(
        projections[0]["content"],
        "user: I prefer quiet accommodation."
    );
    assert_eq!(projections[0]["fragment"]["source_id"], SOURCE);
    assert_eq!(projections[0]["fragment"]["byte_start"], 0);
    assert_eq!(projections[0]["fragment"]["byte_end"], 35);
    // A single synthetic line may have no parser-assigned role. Preserve the
    // independently reconstructed value instead of inventing an author.
    let source: SourceExpectation = serde_json::from_value(payload()["source"].clone()).unwrap();
    let expected = expected_bytes::reconstruct(&state, &source).unwrap();
    assert_eq!(
        projections[0]["fragment"]["role"],
        json!(expected[1].fragment.as_ref().unwrap().role.as_deref())
    );
    let (_, legacy) = call(state.clone(), payload()).await;
    assert_eq!(legacy["schema_version"], 1);
    assert!(legacy.get("fragment_projections").is_none());
    let (disabled, _) = call_route(
        state.clone(),
        payload().to_string(),
        route,
        false,
        Some("fixture"),
    )
    .await;
    assert_eq!(disabled, StatusCode::NOT_FOUND);
    let (unauthenticated, unauthenticated_body) =
        call_route(state, payload().to_string(), route, true, None).await;
    assert_eq!(unauthenticated, StatusCode::UNAUTHORIZED);
    assert!(!unauthenticated_body
        .to_string()
        .contains("quiet accommodation"));
}

#[tokio::test]
async fn v2_projection_uses_sanitized_source_not_raw_request() {
    let state = fixture();
    let mut input = payload();
    input["source"]["content"] = json!("user: Contact sample@example.com for a quiet room.");
    persist(&state, &input).await;
    state.tantivy.lock().await.commit().unwrap();
    let (status, proof) = call_route(
        state,
        input.to_string(),
        "/v1/evaluation/ingest-readiness-v2",
        true,
        Some("fixture"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(proof["ready"], true);
    assert!(!proof.to_string().contains("sample@example.com"));
    assert!(!proof["fragment_projections"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn missing_fact_and_foreign_stream_never_pass_union() {
    let state = fixture();
    let mut source_only = payload();
    source_only["facts"] = json!([]);
    persist(&state, &source_only).await;
    state.tantivy.lock().await.commit().unwrap();
    assert_eq!(call(state.clone(), source_only).await.1["ready"], true);
    let (_, missing) = call(state.clone(), payload()).await;
    assert_eq!(missing["ready"], false);
    assert_eq!(missing["records"][2]["chunk"], "unavailable");
    assert_eq!(missing["records"][2]["document"], "missing");
    let mut foreign = payload();
    foreign["stream"] = json!("foreign");
    let (_, proof) = call(state, foreign).await;
    assert_eq!(proof["ready"], false);
    assert!(proof["records"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["chunk"] == "unavailable"));
}

#[tokio::test]
async fn finite_dimension_checked_and_missing_vectors_not_repaired() {
    let state = fixture();
    persist(&state, &payload()).await;
    state.tantivy.lock().await.commit().unwrap();
    state.store.delete_embedding(FACT).unwrap();
    assert_eq!(
        call(state.clone(), payload()).await.1["records"][2]["vector"],
        "missing"
    );
    assert!(state.store.get_embedding(FACT).unwrap().is_none());
    for vector in [
        vec![0.1; 383],
        vec![f32::NAN; 384],
        vec![f32::INFINITY; 384],
    ] {
        state.store.store_embedding(FACT, vector).unwrap();
        let (_, proof) = call(state.clone(), payload()).await;
        assert_eq!(proof["ready"], false);
        assert_eq!(proof["records"][2]["vector"], "invalid");
    }
}

#[tokio::test]
async fn exact_fact_content_and_metadata_compared_without_resanitization() {
    let state = fixture();
    let mut input = payload();
    input["facts"][0]["expected"]["content"] = json!("literal &lt;tag&gt; quiet room");
    persist(&state, &input).await;
    state.tantivy.lock().await.commit().unwrap();
    assert_eq!(call(state.clone(), input.clone()).await.1["ready"], true);
    let mut different = input.clone();
    different["facts"][0]["expected"]["content"] = json!("different expected text");
    let (_, proof) = call(state.clone(), different).await;
    assert_eq!(proof["records"][2]["chunk"], "content_mismatch");
    assert_eq!(proof["records"][2]["document"], "content_mismatch");
    for field in ["subject", "attributed_to", "extraction_model"] {
        let mut different = input.clone();
        different["facts"][0]["expected"]["extraction_meta"][field] = json!("other");
        let (_, proof) = call(state.clone(), different).await;
        assert_eq!(proof["ready"], false);
        assert_eq!(proof["records"][2]["chunk"], "metadata_mismatch");
    }
}

#[tokio::test]
async fn malformed_id_linkage_schema_and_lifecycle_are_rejected() {
    let state = fixture();
    for (pointer, value) in [
        ("/facts/0/accepted_ordinal", json!(1)),
        ("/facts/0/allocated_id", json!(SOURCE)),
        ("/facts/0/id", json!("not-uuid")),
        ("/facts/0/expected/source_ids", json!([FACT])),
        (
            "/facts/0/expected/extraction_meta/extracted_from",
            json!(FACT),
        ),
        ("/facts/0/expected/source/agent", json!("foreign")),
        (
            "/facts/0/expected/metadata/_loomem_ingest/conversation_date",
            json!("2024-03-21"),
        ),
        ("/facts/0/expected/score", json!(null)),
        ("/facts/0/expected/importance", json!(null)),
        ("/facts/0/expected/is_latest", json!(false)),
        ("/facts/0/expected/deleted_at", json!(1)),
        ("/source/fragment_ids", json!([])),
    ] {
        let mut bad = payload();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert!(
            call(state.clone(), bad).await.0.is_client_error(),
            "{pointer}"
        );
    }
    for field in ["ingester_user_id", "extra"] {
        let mut bad = payload();
        bad["facts"][0]["expected"][field] = json!("rejected");
        assert!(call(state.clone(), bad).await.0.is_client_error());
    }
    let mut bad = payload();
    bad["facts"][0]["expected"]["extraction_meta"]["extra"] = json!("rejected");
    assert!(call(state.clone(), bad).await.0.is_client_error());
    let mut bad = payload();
    bad["facts"][0]["expected"]
        .as_object_mut()
        .unwrap()
        .remove("fragment");
    assert!(call(state, bad).await.0.is_client_error());
}

#[tokio::test]
async fn auth_opt_in_size_and_other_mutator_guards_still_apply() {
    let mut state = fixture();
    let path = "/v1/evaluation/ingest-readiness";
    assert_eq!(
        call_route(
            state.clone(),
            payload().to_string(),
            path,
            false,
            Some("fixture")
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call_route(state.clone(), payload().to_string(), path, true, None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call_route(
            state.clone(),
            " ".repeat(MAX_BYTES + 1),
            path,
            true,
            Some("fixture")
        )
        .await
        .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    Arc::get_mut(&mut state)
        .unwrap()
        .config
        .entity_extraction
        .enabled = true;
    assert_eq!(
        call(state, payload()).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}
