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
    if support.is_empty() {
        return state
            .graph
            .get_or_create_entity(name, "Person", &[], STREAM)
            .unwrap();
    }
    let mut result = None;
    for chunk in support {
        result = Some(
            state
                .graph
                .link_entity(loomem_core::graph::EntityContribution {
                    name,
                    entity_type: "Person",
                    aliases: &[],
                    stream: STREAM,
                    chunk_id: &chunk.id,
                    expected_content: &chunk.content,
                    pii: &state.pii_filter,
                })
                .unwrap(),
        );
    }
    result.unwrap()
}

fn edge(state: &AppState, left: &EntityNode, right: &EntityNode, support: &[&Chunk]) -> Edge {
    let mut result = None;
    for chunk in support {
        result = Some(
            state
                .graph
                .link_relation(loomem_core::graph::RelationContribution {
                    source_entity_id: &left.id,
                    target_entity_id: &right.id,
                    relation: "knows",
                    stream: STREAM,
                    chunk_id: &chunk.id,
                    expected_content: &chunk.content,
                    pii: &state.pii_filter,
                })
                .unwrap(),
        );
    }
    result.unwrap()
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

fn named_contribution(
    state: &AppState,
    name: &str,
    aliases: &[String],
    support: &Chunk,
) -> EntityNode {
    state
        .graph
        .link_entity(loomem_core::graph::EntityContribution {
            name,
            entity_type: "Organization",
            aliases,
            stream: STREAM,
            chunk_id: &support.id,
            expected_content: &support.content,
            pii: &state.pii_filter,
        })
        .unwrap()
}

#[tokio::test]
async fn mixed_entity_hides_deleted_only_labels_and_query_aliases_without_cleanup() {
    let (app, state) = crate::tests::make_test_app();
    let source = source(&state);
    let removed = fact(&state, &source);
    let live = chunk();
    state.store.store_chunk(&live).unwrap();
    let old_aliases = vec!["RetiredAlias".into(), "SharedIdentity".into()];
    let old = named_contribution(&state, "FormerCanonical", &old_aliases, &removed);
    let live_aliases = vec!["SharedIdentity".into(), "CurrentAlias".into()];
    let retained = named_contribution(&state, "RetainedCanonical", &live_aliases, &live);
    assert_eq!(
        old.id, retained.id,
        "fixture must exercise mixed support on one entity"
    );
    let deleted_neighbor = entity(&state, "DeletedNeighbor", &[&removed]);
    edge(&state, &old, &deleted_neighbor, &[&removed]);
    state
        .store
        .tombstone_ingest_source(&source.id, 2000)
        .unwrap();
    let text = mcp(&state, "CurrentAlias").await;
    for removed_label in ["FormerCanonical", "RetiredAlias", "DeletedNeighbor"] {
        assert!(!text.contains(removed_label));
    }
    assert!(text.contains("RetainedCanonical"));
    assert!(text.contains("CurrentAlias"));
    let (status, body) = http(app.clone(), "CurrentAlias").await;
    assert!(status.is_success());
    assert!(!body.contains(&removed.id));
    assert!(body.contains(&live.id));
    for query in ["FormerCanonical", "RetiredAlias"] {
        let (status, body) = http(app.clone(), query).await;
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(!body.contains("RetainedCanonical"));
        assert!(state
            .graph
            .find_live_related_chunks(&[query.into()], 2, STREAM)
            .unwrap()
            .is_empty());
    }
    let candidates = state
        .graph
        .find_live_related_chunks(&["CurrentAlias".into()], 2, STREAM)
        .unwrap();
    assert!(candidates.iter().any(|(id, _)| id == &live.id));
    assert!(!candidates.iter().any(|(id, _)| id == &removed.id));
}

#[test]
fn legacy_alias_without_contribution_cannot_select_an_entity() {
    let (_, state) = crate::tests::make_test_app();
    let support = chunk();
    state.store.store_chunk(&support).unwrap();
    let legacy = state
        .graph
        .get_or_create_entity(
            "LegacyCanonical",
            "Organization",
            &["UnprovenAlias".into()],
            STREAM,
        )
        .unwrap();
    state
        .graph
        .add_chunk_to_entity(&legacy.id, &support.id)
        .unwrap();
    assert!(state
        .graph
        .get_live_entity_by_name("UnprovenAlias", STREAM)
        .unwrap()
        .is_none());
    if let Some(projected) = state.graph.get_live_entity_by_id(&legacy.id).unwrap() {
        assert!(projected.aliases.is_empty());
    }
}

#[test]
fn reverse_index_reader_hides_missing_corrupt_and_cross_stream_memberships() {
    let (_, state) = crate::tests::make_test_app();
    let support = chunk();
    state.store.store_chunk(&support).unwrap();
    let linked = entity(&state, "SupportedEntity", &[&support]);
    assert_eq!(entity_ids_for_chunk(&state, &support), vec![linked.id]);
    let mut outside = support.clone();
    outside.stream = "outside".into();
    assert!(entity_ids_for_chunk(&state, &outside).is_empty());
    state
        .store
        .put(
            format!("graph:chunk:{}", support.id).as_bytes(),
            b"invalid-index",
        )
        .unwrap();
    assert!(entity_ids_for_chunk(&state, &support).is_empty());
}

#[tokio::test]
async fn corrupt_label_proof_never_falls_back_to_aggregate_aliases() {
    let (app, state) = crate::tests::make_test_app();
    let support = chunk();
    state.store.store_chunk(&support).unwrap();
    let aliases = vec!["SupportedAlias".into()];
    let linked = named_contribution(&state, "SupportedCanonical", &aliases, &support);
    assert!(http(app.clone(), "SupportedAlias").await.0.is_success());
    state
        .store
        .put(
            format!("graph:labels:{}:{}", linked.id, support.id).as_bytes(),
            b"invalid-proof",
        )
        .unwrap();
    let (status, body) = http(app, "SupportedAlias").await;
    assert!(!status.is_success());
    assert!(!body.contains("SupportedCanonical"));
    let candidates = state
        .graph
        .find_live_related_chunks(&["SupportedAlias".into()], 1, STREAM);
    assert!(candidates.is_err() || candidates.unwrap().is_empty());
    assert!(entities(&state).is_empty());
}

#[tokio::test]
async fn shared_relation_keeps_live_support_after_one_contributor_is_deleted() {
    let (app, state) = crate::tests::make_test_app();
    let source = source(&state);
    let removed = fact(&state, &source);
    let live = chunk();
    state.store.store_chunk(&live).unwrap();
    let left = entity(&state, "SharedRoot", &[&removed, &live]);
    let right = entity(&state, "SharedTarget", &[&removed, &live]);
    edge(&state, &left, &right, &[&removed, &live]);
    state
        .store
        .tombstone_ingest_source(&source.id, 2000)
        .unwrap();
    let neighbors = state.graph.get_live_neighbors(&left.id).unwrap();
    assert_eq!(neighbors.len(), 1);
    assert_eq!(neighbors[0].0.chunk_ids, vec![live.id.clone()]);
    assert_eq!(neighbors[0].1.chunk_ids, vec![live.id]);
    let (status, body) = http(app, "SharedRoot").await;
    assert!(status.is_success());
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["neighbors"][0]["entity"], "SharedTarget");
    assert_eq!(body["neighbors"][0]["relation"], "related_to");
    assert!(!body.to_string().contains(&removed.id));
}
