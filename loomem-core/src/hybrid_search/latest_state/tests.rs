//! `[search] latest_state_boost`: intent markers, statement times, the
//! recency promotion and the engine no-op paths. Deterministic: fixed
//! timestamps and dates, a tempdir store, no network, no clock.

use super::*;
use crate::hybrid_search::tests::{base_chunk, test_config};
use crate::source_tag::SourceTag;
use crate::storage::RocksDbConfig;

const DAY: u64 = 86_400;
/// 2026-01-01T00:00:00Z.
const T0: u64 = 1_767_225_600;

fn hit(id: &str, score: f64, timestamp: u64) -> HybridSearchResult {
    HybridSearchResult {
        id: id.to_string(),
        content: format!("content of {id}"),
        user_id: "default".to_string(),
        app_id: "default".to_string(),
        level: 1,
        timestamp: i64::try_from(timestamp).unwrap(),
        score,
        bm25_score: 0.5,
        vector_score: 0.5,
        time_decay_factor: 1.0,
    }
}

fn ids(results: &[HybridSearchResult]) -> Vec<&str> {
    results.iter().map(|r| r.id.as_str()).collect()
}

/// Pool shaped like r5 `ho-e-02-q04`: the earlier state ("T-16 approved",
/// session 6) is more similar to the question than its later update ("T-16
/// withdrawn", session 7); an old planning fact anchors the pool start.
fn stale_and_update_pool() -> Vec<HybridSearchResult> {
    vec![
        hit("stale_state", 0.50, T0 + 150 * DAY),
        hit("old_plan", 0.48, T0),
        hit("update", 0.45, T0 + 200 * DAY),
    ]
}

fn times(id: &str) -> Option<u64> {
    stale_and_update_pool()
        .into_iter()
        .find(|r| r.id == id)
        .and_then(|r| u64::try_from(r.timestamp).ok())
}

#[test]
fn current_state_markers_match_the_r5_questions() {
    for query in [
        "How many fish are in my reef tank now, and which ones?",
        "Which dogs have I actually fostered in my home so far, and how many is that?",
        "What ended up happening to the bike Mrs. Okafor donated?",
        "Which trial batches are currently approved for production, and how many is that?",
        "Which jobs are on my waiting list right now, and how many are there?",
        "Which repair tickets were still open on my bench as of our last conversation?",
        "Jakie mam teraz zlecenia w kolejce?",
        "Co wciąż czeka na odbiór?",
        "Ile psów przyjąłem do tej pory?",
        "Jaki jest aktualny stan projektu?",
        "Obecna lista zleceń",
        "Co wciaz czeka na odbior?",
    ] {
        assert!(asks_current_state(query), "{query}");
    }
}

#[test]
fn current_state_markers_need_whole_tokens() {
    for query in [
        "On what date did the ship model's lender approve our facility report?",
        "What was the name of the dog I turned down because of Mabel?",
        "Did the snow knock the stillage over?",
        "Is the far end of the pier open?",
        "",
    ] {
        assert!(!asks_current_state(query), "{query}");
    }
}

#[test]
fn newer_update_outranks_the_earlier_state() {
    let mut pool = stale_and_update_pool();
    assert_eq!(ids(&pool), ["stale_state", "old_plan", "update"]);
    assert!(promote_by_recency(&mut pool, 2.0, times));
    // Three distinct times -> ranks 0, 1, 2 -> positions 0, 0.5, 1.
    // update (newest): 0.45 * 2.0 = 0.90; stale (middle):
    // 0.50 * (1 + 1.0 * 0.5) = 0.75; oldest: 0.48 * 1.0.
    assert_eq!(ids(&pool), ["update", "stale_state", "old_plan"]);
    let update = pool.iter().find(|r| r.id == "update").unwrap();
    assert!((update.score - 0.90).abs() < 1e-12);
    let stale = pool.iter().find(|r| r.id == "stale_state").unwrap();
    assert!((stale.score - 0.75).abs() < 1e-12);
    // The oldest statement keeps its score exactly.
    let old = pool.iter().find(|r| r.id == "old_plan").unwrap();
    assert_eq!(old.score.to_bits(), 0.48_f64.to_bits());
}

#[test]
fn newer_update_wins_when_raw_scores_are_close() {
    let mut pool = vec![
        hit("stale_state", 0.50, T0 + 150 * DAY),
        hit("old_plan", 0.40, T0),
        hit("update", 0.48, T0 + 200 * DAY),
    ];
    let lookup = |id: &str| match id {
        "stale_state" => Some(T0 + 150 * DAY),
        "old_plan" => Some(T0),
        "update" => Some(T0 + 200 * DAY),
        _ => None,
    };
    assert!(promote_by_recency(&mut pool, 1.5, lookup));
    assert_eq!(ids(&pool), ["update", "stale_state", "old_plan"]);
}

/// Rank, not linear time: a far-old wall-clock fallback and a `9999-12-31`
/// date would squeeze the real dates (150 vs 200 days into a ~8000-year
/// span) to nearly the same linear position. By rank the four distinct
/// times sit at 0, 1/3, 2/3, 1, so the update keeps its full lead.
#[test]
fn outlier_dates_do_not_compress_the_real_ones() {
    let far_future = day_start_unix("9999-12-31").unwrap();
    let mut pool = vec![
        hit("stale_state", 0.50, T0 + 150 * DAY),
        hit("update", 0.45, T0 + 200 * DAY),
        hit("far_old", 0.30, 1),
        hit("far_future", 0.20, far_future),
    ];
    let lookup = |id: &str| match id {
        "stale_state" => Some(T0 + 150 * DAY),
        "update" => Some(T0 + 200 * DAY),
        "far_old" => Some(1),
        "far_future" => Some(far_future),
        _ => None,
    };
    assert!(promote_by_recency(&mut pool, 2.5, lookup));
    let score = |id: &str| pool.iter().find(|r| r.id == id).unwrap().score;
    // stale: x(1 + 1.5 / 3) = x1.5; update: x(1 + 1.5 * 2 / 3) = x2.0.
    assert!((score("stale_state") - 0.50 * 1.5).abs() < 1e-12);
    assert!((score("update") - 0.45 * 2.0).abs() < 1e-12);
    assert!((score("update") / score("stale_state") - 0.45 * 2.0 / 0.75).abs() < 1e-12);
    assert_eq!(score("far_old").to_bits(), 0.30_f64.to_bits());
    assert!((score("far_future") - 0.20 * 2.5).abs() < 1e-12);
    assert_eq!(
        ids(&pool),
        ["update", "stale_state", "far_future", "far_old"]
    );
}

#[test]
fn non_positive_scores_keep_their_value() {
    let mut pool = stale_and_update_pool();
    pool.push(hit("negative", -0.10, T0 + 300 * DAY));
    pool.push(hit("zero", 0.0, T0 + 300 * DAY));
    let lookup = |id: &str| match id {
        "negative" | "zero" => Some(T0 + 300 * DAY),
        other => times(other),
    };
    assert!(promote_by_recency(&mut pool, 2.0, lookup));
    let score = |id: &str| pool.iter().find(|r| r.id == id).unwrap().score;
    assert_eq!(score("negative").to_bits(), (-0.10_f64).to_bits());
    assert_eq!(score("zero").to_bits(), 0.0_f64.to_bits());
    // Positive scores still promoted (4 distinct times: update is rank 2/3).
    assert!(score("update") > 0.45);
    assert_eq!(&ids(&pool)[3..], ["zero", "negative"]);
}

#[test]
fn equal_times_leave_scores_and_order_untouched() {
    let mut pool = stale_and_update_pool();
    let before: Vec<(String, u64)> = pool
        .iter()
        .map(|r| (r.id.clone(), r.score.to_bits()))
        .collect();
    assert!(!promote_by_recency(&mut pool, 2.0, |_| Some(T0)));
    let after: Vec<(String, u64)> = pool
        .iter()
        .map(|r| (r.id.clone(), r.score.to_bits()))
        .collect();
    assert_eq!(before, after);
}

#[test]
fn missing_times_stay_neutral() {
    let mut pool = stale_and_update_pool();
    pool.push(hit("no_meta", 0.47, 0));
    // No candidate has a time: nothing to rank by.
    assert!(!promote_by_recency(&mut pool, 2.0, |_| None));
    // Only `no_meta` lacks a time: its score is kept bit for bit.
    let lookup = |id: &str| if id == "no_meta" { None } else { times(id) };
    assert!(promote_by_recency(&mut pool, 2.0, lookup));
    let no_meta = pool.iter().find(|r| r.id == "no_meta").unwrap();
    assert_eq!(no_meta.score.to_bits(), 0.47_f64.to_bits());
}

fn ingest_fact(conversation_date: Option<&str>) -> Chunk {
    let source_id = "00000000-0000-4000-8000-000000000001".to_string();
    let mut chunk = base_chunk();
    chunk.level = 1;
    chunk.timestamp = T0;
    chunk.source = Some(SourceTag::from_agent("mcp-ingest"));
    chunk.trust_level = Some("a2".to_string());
    chunk.source_ids = Some(vec![source_id.clone()]);
    chunk.metadata = Some(serde_json::json!({"_loomem_ingest": {
        "version": 1, "kind": "fact", "conversation_date": conversation_date}}));
    chunk.extraction_meta = Some(
        serde_json::from_value(serde_json::json!({"fact_type": "event", "subject": null,
            "event_date": null, "event_date_context": null, "supersedes": null,
            "superseded_by": null, "confidence": 1.0, "extracted_from": source_id,
            "extraction_model": null}))
        .unwrap(),
    );
    assert!(is_ingest_fact_shape(&chunk));
    chunk
}

fn ingest_fragment(conversation_date: Option<&str>) -> Chunk {
    let source_id = "00000000-0000-4000-8000-000000000002".to_string();
    let mut chunk = base_chunk();
    chunk.id = format!("{source_id}:f0");
    chunk.timestamp = T0;
    chunk.consolidated = true;
    chunk.source = Some(SourceTag::from_agent("mcp-ingest"));
    chunk.source_ids = Some(vec![source_id.clone()]);
    chunk.metadata =
        Some(serde_json::json!({"_loomem_ingest": {"version": 1, "kind": "fragment"}}));
    chunk.fragment = Some(crate::fragments::FragmentMeta {
        source_id,
        byte_start: 0,
        byte_end: 10,
        turn_index: 0,
        role: Some("user".to_string()),
        conversation_date: conversation_date.map(str::to_string),
    });
    assert!(is_ingest_fragment(&chunk));
    chunk
}

#[test]
fn statement_time_prefers_the_ingest_conversation_date() {
    // 2025-11-20T00:00:00Z
    assert_eq!(
        statement_time(&ingest_fact(Some("2025-11-20"))),
        Some(1_763_596_800)
    );
    assert_eq!(
        statement_time(&ingest_fragment(Some("2025-11-20"))),
        Some(1_763_596_800)
    );
    // No date recorded: the chunk timestamp.
    assert_eq!(statement_time(&ingest_fact(None)), Some(T0));
    assert_eq!(statement_time(&ingest_fragment(None)), Some(T0));
    // A present but malformed or non-canonical date is unknown, not a
    // timestamp fallback.
    assert_eq!(statement_time(&ingest_fact(Some("20.11.2025"))), None);
    assert_eq!(statement_time(&ingest_fact(Some("2025-11-2"))), None);
    assert_eq!(statement_time(&ingest_fact(Some(" 2025-11-20"))), None);
}

#[test]
fn statement_time_ignores_forged_dates_and_superseded_chunks() {
    // Not the server-owned ingest shape: the metadata date is ignored.
    let mut forged = ingest_fact(Some("2020-01-01"));
    forged.source = None;
    assert_eq!(statement_time(&forged), Some(T0));
    let mut plain = base_chunk();
    assert_eq!(statement_time(&plain), None, "timestamp 0 is no time");
    plain.timestamp = T0;
    assert_eq!(statement_time(&plain), Some(T0));
    let mut old = ingest_fact(Some("2025-11-20"));
    old.is_latest = false;
    assert_eq!(statement_time(&old), None);
    let mut replaced = ingest_fact(Some("2025-11-20"));
    replaced.superseded_by = Some("newer".to_string());
    assert_eq!(statement_time(&replaced), None);
}

fn store_with(chunks: &[Chunk]) -> (tempfile::TempDir, RocksDbStore) {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = RocksDbConfig {
        max_open_files: 100,
        compression: "lz4".to_string(),
        write_buffer_size: 4 * 1024 * 1024,
        max_write_buffer_number: 2,
    };
    let store = RocksDbStore::open(dir.path(), &cfg).unwrap();
    for chunk in chunks {
        store.store_chunk(chunk).unwrap();
    }
    (dir, store)
}

fn stored_pool() -> (tempfile::TempDir, RocksDbStore) {
    let chunks: Vec<Chunk> = stale_and_update_pool()
        .iter()
        .map(|r| {
            let mut chunk = base_chunk();
            chunk.id = r.id.clone();
            chunk.content = r.content.clone();
            chunk.timestamp = u64::try_from(r.timestamp).unwrap();
            chunk
        })
        .collect();
    store_with(&chunks)
}

fn engine(boost: f64) -> HybridSearchEngine {
    let mut config = test_config();
    config.search.latest_state_boost = boost;
    HybridSearchEngine::new(config)
}

fn bits(results: &[HybridSearchResult]) -> Vec<(String, u64)> {
    results
        .iter()
        .map(|r| (r.id.clone(), r.score.to_bits()))
        .collect()
}

#[test]
fn default_boost_is_byte_identical() {
    let (_dir, store) = stored_pool();
    let query = "Which trial batches are currently approved?";
    let mut pool = stale_and_update_pool();
    let before = bits(&pool);
    engine(1.0).promote_latest_state(query, &mut pool, &store);
    assert_eq!(bits(&pool), before);
    // The shipped config.toml default (1.0) is asserted by
    // `config::search_validation::tests::shipped_config_keeps_every_lever_off`.
}

#[test]
fn non_current_state_query_is_byte_identical() {
    let (_dir, store) = stored_pool();
    let mut pool = stale_and_update_pool();
    let before = bits(&pool);
    engine(2.0).promote_latest_state("When was T-16 approved?", &mut pool, &store);
    assert_eq!(bits(&pool), before);
}

#[test]
fn engine_promotes_the_stored_update_for_a_current_state_query() {
    let (_dir, store) = stored_pool();
    let mut pool = stale_and_update_pool();
    engine(2.0).promote_latest_state(
        "Which trial batches are currently approved?",
        &mut pool,
        &store,
    );
    assert_eq!(ids(&pool), ["update", "stale_state", "old_plan"]);
}

#[test]
fn engine_keeps_unknown_ids_neutral() {
    let (_dir, store) = stored_pool();
    let mut pool = stale_and_update_pool();
    pool.push(hit("not_in_store", 0.99, T0));
    engine(2.0).promote_latest_state("What is current?", &mut pool, &store);
    assert_eq!(pool[0].id, "not_in_store");
    assert_eq!(pool[0].score.to_bits(), 0.99_f64.to_bits());
}
