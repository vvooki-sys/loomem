//! Integration test for the stream-statistics feature (brief: stream stats
//! endpoint). loomem-server has no `[lib]` target, so — like
//! `feedback_mcp_test.rs` — this drives the core layer
//! (`loomem_core::stream_stats`) that the REST handlers and the MCP
//! `memory_stats` tool wrap, over a real tempdir-backed RocksDB and a real
//! event-log directory.
//!
//! Coverage: end-to-end aggregation across every section (REST/user path),
//! the admin all-streams `_total` aggregate, the MCP text rendering, and — the
//! load-bearing one — the **privacy invariant**: no chunk content ever appears
//! in the JSON or text output.

use std::io::Write;

use tempfile::TempDir;

use loomem_core::storage::{
    Chunk, ExtractionMeta, FactType, ProvenanceRole, RocksDbConfig, RocksDbStore,
};
use loomem_core::stream_stats::{self, ComputeOpts};

const DAY: u64 = 86_400;
/// A long, unique sentinel planted in chunk content + original_content. If it
/// ever surfaces in the stats output, the privacy invariant is broken.
const SECRET: &str = "SUPERSECRET_CONTENT_THAT_MUST_NEVER_LEAK_INTO_STATS_0123456789_abcdefghijklmnopqrstuvwxyz_this_is_well_over_one_hundred_characters_long";

fn rocksdb_cfg() -> RocksDbConfig {
    RocksDbConfig {
        max_open_files: 50,
        compression: "none".into(),
        write_buffer_size: 4 * 1024 * 1024,
        max_write_buffer_number: 2,
    }
}

#[allow(clippy::too_many_arguments)]
fn chunk(
    id: &str,
    stream: &str,
    level: i32,
    fact: Option<FactType>,
    attributed: Option<&str>,
    trust: Option<&str>,
    is_latest: bool,
    deleted: bool,
    consolidated: bool,
) -> Chunk {
    let extraction_meta = fact.map(|fact_type| ExtractionMeta {
        fact_type,
        subject: None,
        event_date: None,
        event_date_context: None,
        supersedes: None,
        superseded_by: None,
        confidence: 1.0,
        extracted_from: None,
        extraction_model: None,
        // Plant the secret in the audit field too — it must never leak.
        original_content: Some(SECRET.to_string()),
        topic: None,
        attributed_to: attributed.map(str::to_string),
    });
    Chunk {
        id: id.to_string(),
        content: SECRET.to_string(),
        stream: stream.to_string(),
        level,
        score: 1.0,
        timestamp: 1000,
        consolidated,
        dormant: false,
        in_progress: false,
        prompt_version: None,
        source_ids: None,
        last_decay: None,
        metadata: None,
        importance: None,
        persistent: false,
        last_implicit_boost: None,
        access_count: 0,
        source: None,
        created_by: None,
        updated_at: None,
        valid_from: None,
        valid_until: None,
        is_latest,
        superseded_by: None,
        supersedes_id: None,
        root_memory_id: None,
        version: 1,
        memory_type: None,
        extraction_meta,
        deleted_at: if deleted { Some(2000) } else { None },
        trust_level: trust.map(str::to_string),
        ingester_user_id: None,
        alpha: 1.0,
        beta: 1.0,
        harmful_count: 0,
        n_ratings: 0,
        last_rated_at: None,
        provenance_role: ProvenanceRole::Claim,
    }
}

/// Populate a store for stream "s1": two live L0 (fact/user/a1 embedded,
/// event/assistant/a2 unconsolidated), one live L1, one deleted, one
/// superseded.
fn seed_s1(store: &RocksDbStore) {
    store
        .store_chunk(&chunk(
            "a",
            "s1",
            0,
            Some(FactType::Fact),
            Some("user"),
            Some("a1"),
            true,
            false,
            true,
        ))
        .unwrap();
    store.store_embedding("a", vec![0.1; 8]).unwrap();
    store
        .store_chunk(&chunk(
            "b",
            "s1",
            0,
            Some(FactType::Event),
            Some("assistant"),
            Some("a2"),
            true,
            false,
            false,
        ))
        .unwrap();
    store
        .store_chunk(&chunk(
            "c",
            "s1",
            1,
            Some(FactType::Fact),
            None,
            Some("a1"),
            true,
            false,
            true,
        ))
        .unwrap();
    store
        .store_chunk(&chunk("d", "s1", 0, None, None, None, true, true, false))
        .unwrap();
    store
        .store_chunk(&chunk("e", "s1", 0, None, None, None, false, false, false))
        .unwrap();
}

fn write_events(dir: &std::path::Path, lines: &[String]) {
    let mut f = std::fs::File::create(dir.join("events.jsonl")).unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}

fn opts(now: u64) -> ComputeOpts {
    ComputeOpts {
        now,
        min_chunks_to_consolidate: 3,
        event_log_enabled: true,
    }
}

/// End-to-end: every section reflects the seeded store + event log.
#[test]
fn compute_stream_covers_all_sections() {
    let tmp = TempDir::new().unwrap();
    let store = RocksDbStore::open(tmp.path(), &rocksdb_cfg()).unwrap();
    seed_s1(&store);

    let events = TempDir::new().unwrap();
    let now = 100 * DAY;
    let recent = now - 100;
    write_events(
        events.path(),
        &[
            format!(
                r#"{{"timestamp":{recent},"event":{{"type":"store","content_len":10,"chunk_count":3,"stream_id":"s1","source":"api"}}}}"#
            ),
            format!(
                r#"{{"timestamp":{recent},"event":{{"type":"store","content_len":0,"chunk_count":0,"stream_id":"s1","source":"api"}}}}"#
            ),
            format!(
                r#"{{"timestamp":{recent},"event":{{"type":"search","query":"q","stream_id":"s1","top_scores":[0.9],"latency_ms":5,"result_count":1}}}}"#
            ),
            format!(
                r#"{{"timestamp":{recent},"event":{{"type":"consolidation","input_count":5,"output_count":1,"dropped_ids":[],"cost_usd":0.01}}}}"#
            ),
        ],
    );

    let s = stream_stats::compute_stream(&store, events.path(), &opts(now), "s1").unwrap();

    // health
    assert_eq!(s.health.memory_count, 3, "3 live chunks (a,b,c)");
    assert_eq!(s.health.deleted_count, 1);
    assert_eq!(s.health.superseded_count, 1);
    assert_eq!(s.health.l0_count, 2);
    assert_eq!(s.health.l1_count, 1);
    assert_eq!(s.health.last_ingest_at, Some(recent));
    assert_eq!(s.health.last_search_at, Some(recent));
    // retrieval
    assert_eq!(s.retrieval.embedded_count, 1);
    assert_eq!(s.retrieval.embeddings_pending, 2);
    assert_eq!(s.retrieval.undecodable_count, 0);
    // consolidation: only chunk "b" is a live, unconsolidated L0
    assert_eq!(s.consolidation.chunks_awaiting_consolidation, 1);
    assert_eq!(s.consolidation.runs_total_global, 1);
    assert_eq!(s.consolidation.last_at_global, Some(recent));
    // distribution
    assert_eq!(s.distribution.fact_types.fact, 2);
    assert_eq!(s.distribution.fact_types.event, 1);
    assert_eq!(s.distribution.attribution.user_authored, 1);
    assert_eq!(s.distribution.attribution.assistant_authored, 1);
    assert_eq!(s.distribution.attribution.unattributed, 1);
    assert_eq!(
        s.distribution.trust_tier.a1 + s.distribution.trust_tier.a2 + s.distribution.trust_tier.b,
        s.health.memory_count
    );
    // activity + extraction
    assert_eq!(s.activity.ingests.last_24h, 2);
    assert_eq!(s.activity.searches.last_24h, 1);
    assert_eq!(s.extraction.empty_extractions_24h, 1);
    assert!((s.extraction.avg_facts_per_ingest_24h - 1.5).abs() < 1e-9);
    assert!(s.meta.event_log_enabled);
    assert!(s.meta.scanned_rows >= 5);
}

/// Admin all-streams path: per-stream entries + a `_total` aggregate.
#[test]
fn compute_all_produces_total() {
    let tmp = TempDir::new().unwrap();
    let store = RocksDbStore::open(tmp.path(), &rocksdb_cfg()).unwrap();
    seed_s1(&store);
    store
        .store_chunk(&chunk(
            "x",
            "s2",
            0,
            Some(FactType::Fact),
            Some("user"),
            Some("a1"),
            true,
            false,
            false,
        ))
        .unwrap();

    let events = TempDir::new().unwrap();
    let now = 100 * DAY;
    let all = stream_stats::compute_all(&store, events.path(), &opts(now)).unwrap();

    assert_eq!(all.streams.get("s1").unwrap().health.memory_count, 3);
    assert_eq!(all.streams.get("s2").unwrap().health.memory_count, 1);
    assert_eq!(all.total.health.memory_count, 4, "s1(3) + s2(1)");
    assert_eq!(all.total.stream_id, "_total");
}

/// Privacy invariant — the load-bearing test. Neither the JSON (REST) nor the
/// text (MCP) output may carry any chunk content: no planted secret, and no
/// string value longer than 100 chars (heuristic for "a chunk leaked").
#[test]
fn output_never_leaks_chunk_content() {
    let tmp = TempDir::new().unwrap();
    let store = RocksDbStore::open(tmp.path(), &rocksdb_cfg()).unwrap();
    seed_s1(&store);
    let events = TempDir::new().unwrap();
    let now = 100 * DAY;

    let s = stream_stats::compute_stream(&store, events.path(), &opts(now), "s1").unwrap();

    // REST shape: serialize as the Json<StreamStats> handler would.
    let value = serde_json::to_value(&s).unwrap();
    let mut long_or_secret = Vec::new();
    collect_suspicious_strings(&value, &mut long_or_secret);
    assert!(
        long_or_secret.is_empty(),
        "stats JSON leaked content: {long_or_secret:?}"
    );

    // MCP shape: the rendered text.
    let text = stream_stats::render_text(&s);
    assert!(
        !text.contains("SUPERSECRET"),
        "rendered text leaked content"
    );
    assert!(text.contains("[health]") && text.contains("live=3"));
}

/// Walk a JSON value; record any string value that is over 100 chars or
/// contains the planted secret. Object keys are schema (safe) — only values
/// are checked.
fn collect_suspicious_strings(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) if s.len() > 100 || s.contains("SUPERSECRET") => {
            out.push(s.clone());
        }
        serde_json::Value::String(_) => {}
        serde_json::Value::Array(a) => {
            for item in a {
                collect_suspicious_strings(item, out);
            }
        }
        serde_json::Value::Object(o) => {
            for val in o.values() {
                collect_suspicious_strings(val, out);
            }
        }
        _ => {}
    }
}
