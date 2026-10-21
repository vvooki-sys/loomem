//! `[search] evidence_fields`: `/v1/search` hits carry the additive
//! `evidence` object only when the flag is on, on both the cold path and
//! the warm cache path, rendered from the stored chunk.
use super::stream_scoping_e2e_tests::{response_projection, seed_one};
use super::SearchRequest;
use crate::AppState;
use loomem_core::fragments::FragmentMeta;
use serde_json::{json, Value};
use std::sync::Arc;

fn app_with_evidence(enabled: bool) -> (axum::Router, Arc<AppState>) {
    let (app, original) = crate::tests::make_test_app();
    drop(app);
    let mut config = original.config.clone();
    config.search.evidence_fields = enabled;
    config.search.cache.enabled = true;
    crate::tests::make_test_app_cfg(config)
}

/// One plain chunk and one fragment chunk, both findable by `budget`; the
/// fragment's source must exist (lineage is validated on store).
async fn seed(state: &Arc<AppState>) {
    seed_one(state, "source-1", "streamA", "whole transcript", None).await;
    seed_one(state, "plain", "streamA", "budget review notes", None).await;
    seed_one(state, "frag", "streamA", "budget meeting moved", None).await;
    let mut chunk = state.store.get_chunk("frag").unwrap().unwrap();
    chunk.source_ids = Some(vec!["source-1".into()]);
    chunk.fragment = Some(FragmentMeta {
        source_id: "source-1".into(),
        byte_start: 7,
        byte_end: 27,
        turn_index: 2,
        role: Some("user".into()),
        conversation_date: Some("2023-03-19".into()),
    });
    state.store.store_chunk(&chunk).unwrap();
}

fn request() -> SearchRequest {
    serde_json::from_value(json!({"query": "budget", "top_k": 10, "stream": "streamA"})).unwrap()
}

fn hit<'a>(value: &'a Value, id: &str) -> &'a Value {
    value["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|hit| hit["id"] == id)
        .unwrap_or_else(|| panic!("missing hit {id}: {value}"))
}

#[tokio::test]
async fn evidence_absent_by_default_on_cold_and_warm_paths() {
    let (_, state) = app_with_evidence(false);
    seed(&state).await;
    for _ in 0..2 {
        let value = response_projection(&state, request()).await;
        assert_eq!(value["results"].as_array().unwrap().len(), 2);
        for hit in value["results"].as_array().unwrap() {
            assert!(hit.get("evidence").is_none(), "{hit}");
        }
    }
}

#[tokio::test]
async fn evidence_present_with_rendered_fields_when_enabled() {
    let (_, state) = app_with_evidence(true);
    seed(&state).await;
    let cold = response_projection(&state, request()).await;
    let warm = response_projection(&state, request()).await;
    assert_eq!(warm["results"][0]["metadata"]["cached"], true);
    for value in [cold, warm] {
        assert_eq!(
            hit(&value, "frag")["evidence"],
            json!({
                "author": "user",
                "conversation_date": "2023-03-19",
                "event_date": null,
                "event_date_provenance": null,
                "source_id": "source-1",
                "byte_start": 7,
                "byte_end": 27
            })
        );
        assert_eq!(
            hit(&value, "plain")["evidence"],
            json!({
                "author": null,
                "conversation_date": null,
                "event_date": null,
                "event_date_provenance": null,
                "source_id": null,
                "byte_start": null,
                "byte_end": null
            })
        );
    }
}

#[tokio::test]
async fn memory_item_carries_the_same_evidence_under_the_same_flag() {
    for enabled in [false, true] {
        let (app, state) = app_with_evidence(enabled);
        seed(&state).await;
        let request = axum::http::Request::builder()
            .uri("/api/memories/frag")
            .header("Authorization", "Bearer admin-token")
            .body(axum::body::Body::empty())
            .unwrap();
        let (status, bytes) = crate::tests::send(app, request).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let item: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(item["id"], "frag");
        if enabled {
            assert_eq!(item["evidence"]["author"], "user");
            assert_eq!(item["evidence"]["byte_end"], 27);
            assert_eq!(item["evidence"].as_object().unwrap().len(), 7);
        } else {
            assert!(item.get("evidence").is_none(), "{item}");
        }
    }
}
