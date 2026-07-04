//! Integration test for configurable MCP `memory_search` top_k limits
//! (roadmap W2): under a config with a raised cap, an explicit `top_k = 40`
//! flows through the clamp into retrieval and yields more than 20 results;
//! under shipped values the behavior is identical to the previously
//! hardcoded 5/20 (normal) and 30/30 (aggregation).
//!
//! Mirrors the dispatcher's control flow (`effective_search_top_k` → search
//! limit) against a real tempdir-backed Tantivy index — no storage mocks.

use tempfile::TempDir;

use loomem_core::config::{McpConfig, TantivyConfig};
use loomem_core::{TantivyIndex, TextDocument};

const DOC_COUNT: usize = 45;

fn seeded_index() -> (TempDir, TantivyIndex) {
    let tmp = TempDir::new().expect("tempdir");
    let cfg = TantivyConfig {
        enabled: true,
        heap_size_mb: 16,
        drift_warn_pct: 5.0,
        auto_rebuild_on_drift: false,
    };
    let mut idx = TantivyIndex::open(tmp.path().join("tantivy"), &cfg).expect("open index");
    for i in 0..DOC_COUNT {
        idx.index_document(TextDocument {
            id: format!("doc-{i}"),
            content: format!("shared retrieval corpus entry number {i}"),
            user_id: "default".to_string(),
            app_id: "default".to_string(),
            level: 0,
            timestamp: 1_000 + i64::try_from(i).expect("small index fits i64"),
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

/// Raised cap admits top_k = 40 end-to-end: the clamp honors the explicit
/// request and retrieval returns more than 20 hits.
#[test]
fn raised_cap_returns_more_than_twenty_results() {
    let (_tmp, idx) = seeded_index();
    let mcp = McpConfig {
        search_default_top_k: 5,
        search_max_top_k: 40,
        aggregation_default_top_k: 30,
        aggregation_max_top_k: 30,
    };

    let top_k = mcp.effective_search_top_k(Some(40), false);
    assert_eq!(top_k, 40, "explicit 40 must survive a cap of 40");

    let results = idx
        .search("shared retrieval corpus", top_k)
        .expect("search ok");
    assert!(
        results.len() > 20,
        "expected > 20 results under the raised cap, got {}",
        results.len()
    );
    assert_eq!(results.len(), 40, "45 matching docs clamped to top_k = 40");
}

/// Shipped values reproduce the previously hardcoded behavior byte-for-byte:
/// explicit 40 clamps to 20, omitted defaults to 5, aggregation to 30/30.
#[test]
fn shipped_values_match_previous_hardcoded_behavior() {
    let (_tmp, idx) = seeded_index();
    let shipped = McpConfig::default();

    let clamped = shipped.effective_search_top_k(Some(40), false);
    assert_eq!(clamped, 20, "shipped cap must clamp 40 → 20 as before");
    let results = idx
        .search("shared retrieval corpus", clamped)
        .expect("search ok");
    assert_eq!(results.len(), 20, "shipped cap yields exactly 20 hits");

    assert_eq!(shipped.effective_search_top_k(None, false), 5);
    assert_eq!(shipped.effective_search_top_k(None, true), 30);
    assert_eq!(shipped.effective_search_top_k(Some(99), true), 30);
}
