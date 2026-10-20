use super::*;
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use loomem_core::{
    source_tag::SourceTag,
    storage::{Chunk, ProvenanceRole, UserRole},
};
use std::sync::Arc;
use tower::ServiceExt;

const STREAM: &str = "graph_fixture";

fn chunk() -> Chunk {
    serde_json::from_value(
        json!({"id":uuid::Uuid::new_v4().to_string(),"content":"synthetic",
        "stream":STREAM,"level":0,"score":1.0,"timestamp":1000,
        "consolidated":false,"dormant":false,"in_progress":false}),
    )
    .unwrap()
}

fn source(state: &AppState) -> Chunk {
    let mut source = chunk();
    source.persistent = true;
    source.consolidated = true;
    source.provenance_role = ProvenanceRole::Evidence;
    source.trust_level = Some("a2".into());
    source.source = Some(SourceTag::from_agent("mcp-ingest-source"));
    source.metadata = Some(json!({"_loomem_ingest":{"version":1,"kind":"source"}}));
    state.store.store_chunk(&source).unwrap();
    source
}

fn fact(state: &AppState, source: &Chunk) -> Chunk {
    let mut fact = chunk();
    fact.level = 1;
    fact.trust_level = Some("a2".into());
    fact.source = Some(SourceTag::from_agent("mcp-ingest"));
    fact.source_ids = Some(vec![source.id.clone()]);
    fact.metadata = Some(json!({"_loomem_ingest":{"version":1,"kind":"fact"}}));
    fact.extraction_meta = Some(
        serde_json::from_value(json!({"fact_type":"fact","subject":null,
        "event_date":null,"event_date_context":null,"supersedes":null,"superseded_by":null,
        "confidence":0.9,"extracted_from":source.id,"extraction_model":null}))
        .unwrap(),
    );
    state.store.store_chunk(&fact).unwrap();
    fact
}

fn entity(state: &AppState, name: &str, support: &[&Chunk]) -> EntityNode {
    let entity = state
        .graph
        .get_or_create_entity(name, "Person", &[], STREAM)
        .unwrap();
    for chunk in support {
        state
            .graph
            .add_chunk_to_entity(&entity.id, &chunk.id)
            .unwrap();
    }
    state.graph.get_entity_by_id(&entity.id).unwrap().unwrap()
}

fn edge(state: &AppState, left: &EntityNode, right: &EntityNode, support: &[&Chunk]) -> Edge {
    let edge = state
        .graph
        .get_or_create_edge(&left.id, &right.id, "knows", STREAM)
        .unwrap();
    for chunk in support {
        state.graph.add_chunk_to_edge(&edge.id, &chunk.id).unwrap();
    }
    state.graph.get_neighbors(&left.id).unwrap()[0].0.clone()
}

async fn mcp(state: &Arc<AppState>, name: &str) -> String {
    let auth = crate::auth::AuthContext::single_stream(
        STREAM,
        UserRole::Reader,
        crate::auth::KeyScope::Private,
        None,
        false,
    );
    crate::mcp::dispatcher::dispatch_tool(
        state,
        "memory_graph",
        json!({"entity":name}),
        STREAM,
        &auth,
    )
    .await
    .unwrap()
    .content[0]
        .text
        .clone()
}

async fn http(app: axum::Router, name: &str) -> (axum::http::StatusCode, String) {
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/v1/graph/entity/{name}?stream={STREAM}"))
                .header("Authorization", "Bearer admin-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn both_entrypoints_hide_source_family_after_tombstone_without_graph_cleanup() {
    let (app, state) = crate::tests::make_test_app();
    let source = source(&state);
    let fact = fact(&state, &source);
    let left = entity(&state, "StaleLeft", &[&source, &fact]);
    let right = entity(&state, "StaleRight", &[&fact]);
    let relation = edge(&state, &left, &right, &[&fact]);
    assert!(mcp(&state, "StaleLeft").await.contains("StaleRight"));
    let (status, body) = http(app.clone(), "StaleLeft").await;
    assert!(status.is_success());
    assert!(body.contains(&fact.id));
    state
        .store
        .tombstone_ingest_source(&source.id, 2000)
        .unwrap();
    // Deliberately leave both graph metadata and reverse indexes untouched.
    assert!(state.graph.get_entity_by_id(&left.id).unwrap().is_some());
    assert_eq!(state.graph.get_neighbors(&left.id).unwrap().len(), 1);
    let mcp = mcp(&state, "StaleLeft").await;
    let (status, body) = http(app, "StaleLeft").await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    for secret in [
        &left.canonical_name,
        &right.canonical_name,
        &relation.relation_type,
        &source.id,
        &fact.id,
    ] {
        assert!(!mcp.contains(secret), "MCP leaked {secret}");
        assert!(!body.contains(secret), "HTTP leaked {secret}");
    }
}

#[tokio::test]
async fn surviving_entity_exposes_only_live_ids_and_no_deleted_relation() {
    let (app, state) = crate::tests::make_test_app();
    let source = source(&state);
    let fact = fact(&state, &source);
    let live = chunk();
    state.store.store_chunk(&live).unwrap();
    let left = entity(&state, "Survivor", &[&fact, &live]);
    let right = entity(&state, "DeletedNeighbor", &[&fact]);
    edge(&state, &left, &right, &[&fact]);
    state
        .store
        .tombstone_ingest_source(&source.id, 2000)
        .unwrap();
    let text = mcp(&state, "Survivor").await;
    assert!(text.contains("Linked chunks: 1"));
    assert!(!text.contains("DeletedNeighbor"));
    let (status, body) = http(app, "Survivor").await;
    assert!(status.is_success());
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["chunk_ids"], json!([live.id]));
    assert_eq!(body["neighbors"], json!([]));
}

#[test]
fn missing_cross_stream_corrupt_support_and_mismatched_endpoints_fail_closed() {
    let (_app, state) = crate::tests::make_test_app();
    let live = chunk();
    state.store.store_chunk(&live).unwrap();
    let left = entity(&state, "VisibleRoot", &[&live]);
    let right = entity(&state, "VisibleTarget", &[&live]);
    let mut relation = edge(&state, &left, &right, &[&live]);
    assert!(supported_neighbor(&state, &left, relation.clone(), right.clone()).is_some());
    relation.source_entity_id = "unrelated".into();
    assert!(supported_neighbor(&state, &left, relation.clone(), right.clone()).is_none());
    relation.source_entity_id = left.id.clone();
    relation.stream_id = "foreign".into();
    assert!(supported_neighbor(&state, &left, relation, right).is_none());
    assert!(!live_support(&state, "absent", STREAM));
    assert!(!live_support(&state, &live.id, "foreign"));
    let unsupported = entity(&state, "NoSupport", &[]);
    assert!(supported_entity(&state, unsupported, STREAM).is_none());
    state.store.put(b"chunk:L0:unreadable", b"corrupt").unwrap();
    assert!(!live_support(&state, "unreadable", STREAM));
}
