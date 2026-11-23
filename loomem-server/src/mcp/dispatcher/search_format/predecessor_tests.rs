//! `[mcp] search_include_predecessor`: an updated hit gains one
//! `↳ previously` line with the version it superseded. Off, the
//! `memory_search` text is byte-identical to the text with the lever on minus
//! those lines; the predecessor is charged to `max_context_tokens`, never
//! crosses streams, is skipped for aggregations and when already a hit.
use super::super::tool_search;
use super::{cap_chars, render_line, token_cost, Predecessor};
use crate::handlers::types::SearchResult;
use crate::AppState;
use loomem_core::llm_ner::estimate_tokens;
use loomem_core::storage::Chunk;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const STREAM: &str = "predecessor";
const OTHER_STREAM: &str = "predecessor-other";
const RECORDED: i64 = 1_704_067_200; // 2024-01-01
const EARLIER: i64 = 1_701_388_800; // 2023-12-01
const OLD_TEXT: &str = "predfixture budget was ten thousand";
const NEW_TEXT: &str = "predfixture budget is twelve grand";
const PLAIN_TEXT: &str = "predfixture unrelated quick note";

fn state(include_predecessor: bool, max_chars: usize) -> Arc<AppState> {
    let (_, original) = crate::tests::make_test_app();
    let mut config = original.config.clone();
    config.mcp.search_include_predecessor = include_predecessor;
    config.mcp.predecessor_max_chars = max_chars;
    crate::tests::make_test_app_cfg(config).1
}

fn chunk(id: &str, content: &str, stream: &str, ts: i64) -> Chunk {
    serde_json::from_value(json!({
        "id": id, "content": content,
        "stream": stream, "level": 0, "score": 1.0, "timestamp": ts,
        "consolidated": false, "dormant": false, "in_progress": false
    }))
    .unwrap()
}

async fn persist(state: &Arc<AppState>, chunk: Chunk) {
    let content = chunk.content.clone();
    let stream = chunk.stream.clone();
    let ts = i64::try_from(chunk.timestamp).unwrap();
    crate::handlers::ingest::persist_chunk(
        state, chunk, &content, "default", "default", &stream, 0, ts, None, None,
    )
    .await
    .unwrap();
}

/// `old` (no longer latest) superseded by `new`, plus an unrelated `plain`
/// note. The predecessor lives in `pred_stream` and may be soft-deleted.
async fn seed(state: &Arc<AppState>, pred_stream: &str, pred_deleted: bool) {
    let mut old = chunk("old", OLD_TEXT, pred_stream, EARLIER);
    old.is_latest = false;
    old.deleted_at = pred_deleted.then_some(1_704_000_000);
    persist(state, old).await;
    let mut new = chunk("new", NEW_TEXT, STREAM, RECORDED);
    new.supersedes_id = Some("old".into());
    persist(state, new).await;
    persist(state, chunk("plain", PLAIN_TEXT, STREAM, RECORDED)).await;
}

async fn search(state: &Arc<AppState>, args: Value) -> String {
    let result = tool_search(state, args, STREAM).await.unwrap();
    assert_ne!(result.is_error, Some(true));
    let text = result.content[0].text.clone();
    regex::Regex::new(r"\(score: [0-9.]+\)")
        .unwrap()
        .replace_all(&text, "(score: #)")
        .into_owned()
}

fn hit_ids(text: &str) -> Vec<String> {
    regex::Regex::new(r"\(id: ([^)]+)\)")
        .unwrap()
        .captures_iter(text)
        .map(|c| c[1].to_string())
        .collect()
}

fn without_predecessor_lines(text: &str) -> String {
    text.split_inclusive('\n')
        .filter(|line| !line.starts_with("   ↳ previously "))
        .collect()
}

const PRED_LINE: &str = "   ↳ previously [2023-12-01]: predfixture budget was ten thousand\n";

#[tokio::test]
async fn off_is_the_on_text_without_predecessor_lines() {
    let args = json!({"query": "predfixture", "top_k": 5});
    let off = state(false, 400);
    seed(&off, STREAM, false).await;
    let off_text = search(&off, args.clone()).await;
    let on = state(true, 400);
    seed(&on, STREAM, false).await;
    let on_text = search(&on, args).await;

    assert!(!off_text.contains('↳'), "{off_text}");
    assert_eq!(hit_ids(&on_text).len(), 2, "{on_text}");
    assert_eq!(on_text.matches('↳').count(), 1, "{on_text}");
    assert_eq!(without_predecessor_lines(&on_text), off_text);

    let new_line = on_text
        .split_inclusive('\n')
        .position(|l| l.ends_with("(id: new)\n"))
        .unwrap();
    let lines: Vec<&str> = on_text.split_inclusive('\n').collect();
    assert!(lines[new_line].contains("[UPDATED — supersedes older version]"));
    assert_eq!(lines[new_line + 1], PRED_LINE);
}

#[tokio::test]
async fn predecessor_text_is_capped_with_an_ellipsis() {
    let on = state(true, 11);
    seed(&on, STREAM, false).await;
    let text = search(&on, json!({"query": "predfixture", "top_k": 5})).await;
    assert!(
        text.contains("   ↳ previously [2023-12-01]: predfixture…\n"),
        "{text}"
    );
}

#[tokio::test]
async fn predecessor_counts_toward_max_context_tokens() {
    // Both hits cost the same; a budget of exactly two hits keeps both with
    // the lever off, and only the first once the predecessor line is charged.
    let unit = estimate_tokens(NEW_TEXT) + super::super::RESULT_FORMAT_TOKEN_OVERHEAD;
    assert_eq!(unit, estimate_tokens(PLAIN_TEXT) + 24);
    let args = json!({"query": "predfixture", "top_k": 5, "max_context_tokens": 2 * unit});

    let off = state(false, 400);
    seed(&off, STREAM, false).await;
    assert_eq!(hit_ids(&search(&off, args.clone()).await).len(), 2);

    let on = state(true, 400);
    seed(&on, STREAM, false).await;
    let text = search(&on, args).await;
    assert_eq!(hit_ids(&text).len(), 1, "{text}");
    assert!(text.starts_with("Found 1 relevant memories:"), "{text}");
}

#[tokio::test]
async fn predecessor_in_another_stream_is_not_rendered() {
    let on = state(true, 400);
    seed(&on, OTHER_STREAM, false).await;
    let text = search(&on, json!({"query": "predfixture", "top_k": 5})).await;
    assert!(hit_ids(&text).contains(&"new".to_string()), "{text}");
    assert!(!text.contains('↳'), "{text}");
}

#[tokio::test]
async fn soft_deleted_predecessor_is_not_rendered() {
    let on = state(true, 400);
    seed(&on, STREAM, true).await;
    let text = search(&on, json!({"query": "predfixture", "top_k": 5})).await;
    assert!(hit_ids(&text).contains(&"new".to_string()), "{text}");
    assert!(!text.contains('↳'), "{text}");
}

#[tokio::test]
async fn aggregation_query_gets_no_predecessor_lines() {
    let on = state(true, 400);
    seed(&on, STREAM, false).await;
    let text = search(&on, json!({"query": "list all predfixture"})).await;
    assert!(text.starts_with("ENUMERATION QUERY:"), "{text}");
    assert!(hit_ids(&text).contains(&"new".to_string()), "{text}");
    assert!(!text.contains('↳'), "{text}");
}

#[tokio::test]
async fn include_superseded_does_not_repeat_a_predecessor_hit() {
    let on = state(true, 400);
    seed(&on, STREAM, false).await;
    let args = json!({"query": "predfixture", "top_k": 5, "include_superseded": true});
    let text = search(&on, args).await;
    let ids = hit_ids(&text);
    assert!(ids.contains(&"old".to_string()), "{text}");
    assert!(ids.contains(&"new".to_string()), "{text}");
    assert!(!text.contains('↳'), "{text}");
}

fn ghost(id: &str, content: &str) -> SearchResult {
    SearchResult {
        id: id.into(),
        content: content.into(),
        score: 0.5,
        metadata: Some(json!({"timestamp": RECORDED})),
        trace_info: None,
        signal_breakdown: None,
        content_type: None,
        content_type_source: None,
        evidence: None,
    }
}

fn pred(text: &str) -> Predecessor {
    Predecessor {
        ts: "2023-12-01".into(),
        text: text.into(),
    }
}

#[test]
fn render_line_appends_the_predecessor_after_the_id() {
    let (_, state) = crate::tests::make_test_app();
    let p = pred("was ten");
    assert_eq!(
        render_line(&state.store, 0, &ghost("g1", "is twelve"), Some(&p)),
        "1. [2024-01-01] [trust:?] is twelve (score: 0.50) (id: g1)\n   \
         ↳ previously [2023-12-01]: was ten\n"
    );
    assert_eq!(
        render_line(&state.store, 0, &ghost("g1", "is twelve"), None),
        "1. [2024-01-01] [trust:?] is twelve (score: 0.50) (id: g1)\n"
    );
}

#[test]
fn token_cost_charges_the_predecessor_only_when_present() {
    let hit = ghost("g1", "is twelve grand");
    let own = estimate_tokens("is twelve grand") + 24;
    assert_eq!(token_cost(&hit, &HashMap::new()), own);
    let preds = HashMap::from([("g1".to_string(), pred("was ten thousand"))]);
    assert_eq!(
        token_cost(&hit, &preds),
        own + estimate_tokens("was ten thousand") + 24
    );
    let other = HashMap::from([("g2".to_string(), pred("was ten thousand"))]);
    assert_eq!(token_cost(&hit, &other), own);
}

#[test]
fn cap_chars_counts_characters_not_bytes() {
    assert_eq!(cap_chars("abcdef", 3), "abc…");
    assert_eq!(cap_chars("abc", 3), "abc");
    assert_eq!(cap_chars("ab", 3), "ab");
    assert_eq!(cap_chars("zażółć gęślą", 4), "zażó…");
    assert_eq!(cap_chars("", 3), "");
}
