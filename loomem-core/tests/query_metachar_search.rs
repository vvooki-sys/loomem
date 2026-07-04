//! Integration test for the query-sanitization brief (roadmap W2):
//! a search whose query carries Tantivy operator metacharacters (`-`, `?`)
//! must succeed and return the indexed fact — never a parse error.
//!
//! Seeds a real tempdir-backed Tantivy index from a JSON fixture (no storage
//! mocks) through the public `loomem_core` API, mirroring what the search
//! handler's BM25 leg executes per stream.

use tempfile::TempDir;

use loomem_core::config::TantivyConfig;
use loomem_core::{TantivyIndex, TextDocument};

#[derive(serde::Deserialize)]
struct Fixture {
    docs: Vec<FixtureDoc>,
}

#[derive(serde::Deserialize)]
struct FixtureDoc {
    id: String,
    content: String,
}

fn load_fixture() -> Fixture {
    let raw = include_str!("fixtures/query_metachar_docs.json");
    serde_json::from_str(raw).expect("fixture must deserialize")
}

fn seeded_index() -> (TempDir, TantivyIndex) {
    let tmp = TempDir::new().expect("tempdir");
    let cfg = TantivyConfig {
        enabled: true,
        heap_size_mb: 16,
        drift_warn_pct: 5.0,
        auto_rebuild_on_drift: false,
    };
    let mut idx = TantivyIndex::open(tmp.path().join("tantivy"), &cfg).expect("open index");
    for d in load_fixture().docs {
        idx.index_document(TextDocument {
            id: d.id,
            content: d.content,
            user_id: "default".to_string(),
            app_id: "default".to_string(),
            level: 0,
            timestamp: 1_000,
            stream: "s1".to_string(),
            entities: None,
            relations: None,
            event_date: None,
            source_agent: None,
        })
        .expect("index doc");
    }
    idx.commit().expect("commit");
    (tmp, idx)
}

/// A natural-language question containing `-` and `?` returns success and
/// non-empty results, and the top hits include the target fact. Fails on the
/// pre-brief behaviour where the BM25 parse error killed the whole search.
#[test]
fn metachar_question_returns_success_and_results() {
    let (_tmp, idx) = seeded_index();

    let query = "quick check - which atmospheric correction algorithm is \
                 implemented in the SIAC_GEE tool?";

    let results = idx
        .search(query, 10)
        .expect("metachar query must not hard-fail the search");
    assert!(!results.is_empty(), "expected non-empty results");
    assert!(
        results.iter().any(|r| r.id == "target"),
        "expected the target fact among results, got: {results:?}"
    );

    // Same contract on the stream-filtered entry point used per-stream by
    // the search handler's BM25 leg.
    let stream_results = idx
        .search_with_stream(query, "s1", 10)
        .expect("stream-filtered metachar query must not hard-fail");
    assert!(stream_results.iter().any(|r| r.id == "target"));
}
