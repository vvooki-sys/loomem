//! `[search] event_date_proximity_boost`: off (0.0) the `/v1/search`
//! envelope is identical to the shipped defaults; on, a legacy chunk whose
//! event date sits inside the query range outranks an equal chunk dated just
//! after it, with its score exactly doubled at weight 1.0.
use super::*;
use crate::auth::{AuthContext, KeyScope, UserRole};
use loomem_core::search::date_proximity;
use serde_json::Value;

const INSIDE: &str = "in2024";
const AFTER: &str = "after2024";

/// Two legacy chunks with identical content (short enough that the prefix
/// dedup never collapses them), dated 2024-06-01 and 2025-01-15.
fn corpus() -> [Chunk; 2] {
    let mut after = chunk(AFTER, Some("2025-01-15"), false);
    let mut inside = chunk(INSIDE, Some("2024-06-01"), false);
    after.content = "orchard harvest".into();
    inside.content = "orchard harvest".into();
    [after, inside]
}

/// A test app on `config` with the corpus indexed. Time decay is off: it
/// reads the wall clock in whole seconds, which would make two responses
/// differ across a second boundary regardless of the lever.
async fn app(weight: Option<f64>) -> axum::Router {
    let (_, original) = make_test_app();
    let mut config = original.config.clone();
    config.search.decay.l0_lambda = 0.0;
    config.search.decay.l1_lambda = 0.0;
    if let Some(weight) = weight {
        config.search.event_date_proximity_boost = weight;
    }
    let (app, state) = make_test_app_cfg(config);
    for item in corpus() {
        seed(&state, &item).await;
    }
    app
}

async fn search(app: &axum::Router, dates: bool) -> Value {
    let mut value = http(app, dates).await;
    value["took_ms"] = json!(0);
    value
}

/// The response with rows in id order. The two chunks tie whenever the
/// multiplier is 1.0, and the fused order of tied rows is not stable across
/// app instances (it differs even with the lever unread), so identity is
/// asserted on every field of every row, not on the tie-break. The raw
/// order must still be by descending score.
fn canonical(mut response: Value) -> Value {
    let scores: Vec<f64> = ranked(&response).into_iter().map(|(_, s)| s).collect();
    assert!(
        scores.windows(2).all(|pair| pair[0] >= pair[1]),
        "{scores:?}"
    );
    let rows = response["results"].as_array_mut().unwrap();
    rows.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    response
}

fn ranked(response: &Value) -> Vec<(String, f64)> {
    response["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            let id = row["id"].as_str().unwrap().to_string();
            (id, row["score"].as_f64().unwrap())
        })
        .collect()
}

fn score(response: &Value, id: &str) -> f64 {
    ranked(response)
        .into_iter()
        .find(|(hit, _)| hit == id)
        .map(|(_, score)| score)
        .unwrap()
}

/// The 2024 range exactly as the handler parses it from the request.
fn range_2024() -> DateFilter {
    let (_, state) = make_test_app();
    let payload: SearchRequest = serde_json::from_value(json!({
        "query":"orchard","streams":["streamA"],"top_k":20,
        "date_from":"2024-01-01","date_to":"2024-12-31"
    }))
    .unwrap();
    let auth = AuthContext::single_stream("streamA", UserRole::Admin, KeyScope::Shared, None, true);
    super::super::super::prepare_query_context(&state, &auth, &payload)
        .unwrap()
        .date_filter
        .unwrap()
}

#[tokio::test]
async fn http_weight_zero_is_identical_to_the_shipped_default() {
    let default_app = app(None).await;
    let off_app = app(Some(0.0)).await;
    for dates in [true, false] {
        let expected = canonical(search(&default_app, dates).await);
        let actual = canonical(search(&off_app, dates).await);
        assert_eq!(actual, expected, "dates={dates}");
        let mut ids: Vec<_> = ranked(&expected).into_iter().map(|(id, _)| id).collect();
        ids.sort();
        assert_eq!(ids, [AFTER, INSIDE], "dates={dates}");
    }
}

#[tokio::test]
async fn http_weight_on_without_a_range_is_identical() {
    let default_app = app(None).await;
    let on_app = app(Some(1.0)).await;
    assert_eq!(
        canonical(search(&on_app, false).await),
        canonical(search(&default_app, false).await)
    );
}

#[tokio::test]
async fn http_weight_one_ranks_the_in_range_chunk_first_with_double_score() {
    let off = search(&app(Some(0.0)).await, true).await;
    let on = search(&app(Some(1.0)).await, true).await;
    // Equal content: the two chunks tie when the lever is off.
    assert_eq!(score(&off, INSIDE), score(&off, AFTER));
    let order: Vec<_> = ranked(&on).into_iter().map(|(id, _)| id).collect();
    assert_eq!(order, [INSIDE, AFTER]);
    // ×2 is exact in IEEE 754, so the doubled score compares bit-for-bit.
    assert_eq!(score(&on, INSIDE), score(&off, INSIDE) * 2.0);
    let after = &corpus()[0];
    let m = proximity_multiplier_for(after, Some(&range_2024()), 1.0);
    assert!(m > 1.0 && m < 2.0, "{m}");
    assert!((score(&on, AFTER) - score(&off, AFTER) * m).abs() < 1e-9);
}

#[test]
fn adapter_reads_event_date_and_is_neutral_without_it() {
    let range = range_2024();
    let [after, inside] = corpus();
    assert_eq!(proximity_multiplier_for(&inside, Some(&range), 1.0), 2.0);
    assert_eq!(proximity_multiplier_for(&inside, Some(&range), 0.0), 1.0);
    assert_eq!(proximity_multiplier_for(&inside, None, 1.0), 1.0);
    let DateFilter::Range(start, end) = range;
    let expected = date_proximity::multiplier(Some(1_736_899_200), Some((start, end)), 0.5);
    assert_eq!(
        proximity_multiplier_for(&after, Some(&range), 0.5),
        expected
    );

    let mut undated = inside.clone();
    undated.extraction_meta = None;
    assert_eq!(proximity_multiplier_for(&undated, Some(&range), 1.0), 1.0);
    for raw in ["not a date", "1969-12-31"] {
        let mut odd = inside.clone();
        odd.extraction_meta.as_mut().unwrap().event_date = Some(raw.into());
        assert_eq!(
            proximity_multiplier_for(&odd, Some(&range), 1.0),
            1.0,
            "{raw}"
        );
    }
}
