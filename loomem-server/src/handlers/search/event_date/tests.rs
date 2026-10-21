use super::*;
use crate::tests::{make_test_app, make_test_app_cfg, send};
use axum::{body::Body, http::Request};
use loomem_core::source_tag::SourceTag;
use loomem_core::storage::ProvenanceRole;
use loomem_core::tantivy_index::TextDocument;
use serde_json::json;
use std::sync::Arc;

const RECORDED: u64 = 1_704_067_200;

fn chunk(id: &str, event: Option<&str>, owned: bool) -> Chunk {
    let mut chunk: Chunk = serde_json::from_value(json!({
        "id":id,"content":format!("orchard {id}"),"stream":"streamA",
        "level":0,"score":1.0,"timestamp":RECORDED,
        "consolidated":owned,"dormant":false,"in_progress":false,"persistent":owned,
        "extraction_meta":{"fact_type":"event","event_date":event,"confidence":1.0}
    }))
    .unwrap();
    if owned {
        chunk.metadata = Some(json!({"_loomem_ingest":{"version":1,"kind":"source"}}));
        chunk.provenance_role = ProvenanceRole::Evidence;
        chunk.trust_level = Some("a2".into());
        chunk.source = Some(SourceTag::from_agent("mcp-ingest-source"));
    }
    chunk
}

async fn seed(state: &Arc<AppState>, chunk: &Chunk) {
    state.store.store_chunk(chunk).unwrap();
    let mut index = state.tantivy.lock().await;
    // Deliberately stale legacy index: every row advertises its recorded 2024 date.
    index
        .upsert_document(TextDocument {
            id: chunk.id.clone(),
            content: chunk.content.clone(),
            user_id: "default".into(),
            app_id: "default".into(),
            level: 0,
            timestamp: i64::try_from(RECORDED).unwrap(),
            stream: chunk.stream.clone(),
            entities: None,
            relations: None,
            event_date: Some(i64::try_from(RECORDED).unwrap()),
            source_agent: None,
        })
        .unwrap();
    index.commit().unwrap();
}

async fn http_query(app: &axum::Router, query: &str, dates: bool) -> serde_json::Value {
    let mut payload = json!({"query":query,"streams":["streamA"],"top_k":20});
    if dates {
        payload["date_from"] = json!("2024-01-01");
        payload["date_to"] = json!("2024-12-31");
    }
    let request = Request::builder()
        .method("POST")
        .uri("/v1/search")
        .header("Authorization", "Bearer admin-token")
        .header("Content-Type", "application/json")
        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
        .unwrap();
    let (status, body) = send(app.clone(), request).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    serde_json::from_slice(&body).unwrap()
}

async fn http(app: &axum::Router, dates: bool) -> serde_json::Value {
    http_query(app, "orchard", dates).await
}

fn ids(response: &serde_json::Value) -> Vec<String> {
    let mut ids: Vec<_> = response["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn http_stale_index_unknown_and_wrong_event_dates_cannot_match_recorded_time() {
    let (app, state) = make_test_app();
    for item in [
        chunk("unknown", None, true),
        chunk("wrong", Some("2020-02-03"), true),
        chunk("known", Some("2024-01-01"), true),
        chunk("legacy", None, false),
    ] {
        seed(&state, &item).await;
    }
    assert_eq!(ids(&http(&app, true).await), ["known", "legacy"]);
    assert_eq!(
        ids(&http_query(&app, "orchard 2024-01-01", false).await),
        ["known", "legacy"]
    );
    assert_eq!(
        ids(&http(&app, false).await),
        ["known", "legacy", "unknown", "wrong"]
    );
}

#[tokio::test]
async fn http_warm_cache_rejects_late_stale_recorded_date_results() {
    use crate::auth::{AuthContext, KeyScope, UserRole};
    let (_, original) = make_test_app();
    let mut config = original.config.clone();
    config.search.cache.enabled = true;
    let (app, state) = make_test_app_cfg(config);
    let unknown = chunk("unknown", None, true);
    let known = chunk("known", Some("2024-01-01"), true);
    seed(&state, &unknown).await;
    seed(&state, &known).await;
    let payload: SearchRequest = serde_json::from_value(json!({
        "query":"orchard","streams":["streamA"],"top_k":20,
        "date_from":"2024-01-01","date_to":"2024-12-31"
    }))
    .unwrap();
    let auth = AuthContext::single_stream("streamA", UserRole::Admin, KeyScope::Shared, None, true);
    let ctx = super::super::prepare_query_context(&state, &auth, &payload).unwrap();
    let stale = [unknown, known]
        .into_iter()
        .map(|item| HybridSearchResult {
            id: item.id,
            content: item.content,
            user_id: String::new(),
            app_id: String::new(),
            level: 0,
            timestamp: i64::try_from(RECORDED).unwrap(),
            score: 1.0,
            bm25_score: 1.0,
            vector_score: 0.0,
            time_decay_factor: 1.0,
        })
        .collect();
    // Model a late in-flight cache publish using the old recorded-date index.
    state
        .query_cache
        .lock()
        .await
        .insert(ctx.cache_key.unwrap(), stale);
    let response = http(&app, true).await;
    assert_eq!(ids(&response), ["known"]);
    assert_eq!(response["results"][0]["metadata"]["cached"], true);
    assert_eq!(state.query_cache.lock().await.len(), 1);
}

#[tokio::test]
async fn vector_prefilter_removes_stale_date_candidates_before_ranking() {
    let (_, state) = make_test_app();
    for item in [
        chunk("unknown", None, true),
        chunk("known", Some("2024-01-01"), true),
        chunk("legacy", None, false),
    ] {
        state.store.store_chunk(&item).unwrap();
    }
    let values = vec![
        ("unknown".into(), vec![1.0]),
        ("known".into(), vec![0.5]),
        ("legacy".into(), vec![0.1]),
    ];
    let filter = DateFilter::Range(1_704_067_200, 1_735_689_599);
    let filtered = embeddings(values.clone(), Some(&filter), &state);
    assert_eq!(
        filtered
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["known", "legacy"]
    );
    assert_eq!(embeddings(values.clone(), None, &state), values);
}

#[test]
fn strict_date_guard_preserves_unknown_and_legacy_without_event_query() {
    let filter = DateFilter::Range(-1, 0);
    assert!(allows(&chunk("legacy", None, false), Some(&filter)));
    assert!(allows(&chunk("unknown", None, true), None));
    assert!(!allows(&chunk("unknown", None, true), Some(&filter)));
    assert!(allows(
        &chunk("epoch", Some("1970-01-01"), true),
        Some(&filter)
    ));
}

#[test]
fn source_linked_fact_shape_uses_event_time_and_spoofed_marker_stays_legacy() {
    let mut fact = chunk("fact", None, true);
    let source = "00000000-0000-4000-8000-000000000001";
    fact.metadata = Some(json!({"_loomem_ingest":{"version":1,"kind":"fact"}}));
    fact.provenance_role = ProvenanceRole::Claim;
    fact.source = Some(SourceTag::from_agent("mcp-ingest"));
    fact.source_ids = Some(vec![source.into()]);
    fact.extraction_meta.as_mut().unwrap().extracted_from = Some(source.into());
    let range = DateFilter::Range(1_704_067_200, 1_735_689_599);
    assert!(is_ingest_fact_shape(&fact));
    assert!(!allows(&fact, Some(&range)));
    fact.extraction_meta.as_mut().unwrap().event_date = Some("2024-02-03".into());
    assert!(allows(&fact, Some(&range)));
    fact.source_ids = None;
    assert!(!is_ingest_fact_shape(&fact));
    assert!(allows(&fact, Some(&DateFilter::Range(-1, 0))));
}

#[test]
fn fragment_with_conversation_date_is_excluded_by_range_like_its_source() {
    use loomem_core::fragments::FragmentMeta;
    use loomem_core::storage::is_ingest_fragment;
    let source_id = "00000000-0000-4000-8000-000000000002";
    let mut source = chunk(source_id, None, true);
    source.metadata = Some(json!({"_loomem_ingest":{
        "version":1,"kind":"source","conversation_date":"2024-02-03"}}));
    let mut fragment = chunk(&format!("{source_id}:f0"), None, true);
    fragment.metadata = Some(json!({"_loomem_ingest":{
        "version":1,"kind":"fragment","conversation_date":"2024-02-03"}}));
    fragment.provenance_role = ProvenanceRole::Evidence;
    fragment.source = Some(SourceTag::from_agent("mcp-ingest"));
    fragment.source_ids = Some(vec![source_id.into()]);
    fragment.extraction_meta = None;
    fragment.fragment = Some(FragmentMeta {
        source_id: source_id.into(),
        byte_start: 0,
        byte_end: 7,
        turn_index: 0,
        role: Some("user".into()),
        conversation_date: Some("2024-02-03".into()),
    });
    assert!(is_ingest_fragment(&fragment));
    // The conversation date is observation time, never event time: a
    // fragment is unknown-dated exactly like its source, in every lane.
    let range = DateFilter::Range(1_704_067_200, 1_735_689_599);
    assert!(!allows(&source, Some(&range)));
    assert!(!allows(&fragment, Some(&range)));
    assert!(allows(&fragment, None));
}
