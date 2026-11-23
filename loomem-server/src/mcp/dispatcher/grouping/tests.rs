//! `[mcp] search_group_by`: `## <key>` headers before each hit that starts a
//! new group, order and numbering unchanged. `none` (and every aggregation)
//! renders exactly `render_lines`; at the `memory_search` level the grouped
//! text minus its header lines is byte-identical to the ungrouped text.
use super::super::search_format::render_lines;
use super::super::tool_search;
use super::{effective_mode, group_key, render_grouped};
use crate::handlers::types::SearchResult;
use crate::AppState;
use loomem_core::config::SearchGroupBy;
use loomem_core::fragments::FragmentMeta;
use loomem_core::source_tag::SourceTag;
use loomem_core::storage::Chunk;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

const STREAM: &str = "grouping";
const DAY1: i64 = 1_704_067_200; // 2024-01-01
const DAY1_LATER: i64 = DAY1 + 3_600; // 2024-01-01, distinct for a stable rank
const DAY2: i64 = 1_704_153_600; // 2024-01-02

/// Test app with `search_group_by = mode` and the LLM key pinned missing, so
/// nothing on the search path can reach a paid provider.
fn state(mode: SearchGroupBy) -> Arc<AppState> {
    let (_, original) = crate::tests::make_test_app();
    let mut config = original.config.clone();
    config.mcp.search_group_by = mode;
    config.llm.api_key = None;
    config.llm.api_key_env = "LOOMEM_TEST_KEY_THAT_IS_NEVER_SET".into();
    config.llm.providers.clear();
    assert!(config.llm.active_provider().is_err());
    crate::tests::make_test_app_cfg(config).1
}

fn chunk(id: &str, content: &str, session: Option<&str>, ts: i64) -> Chunk {
    let mut c: Chunk = serde_json::from_value(json!({
        "id": id, "content": content,
        "stream": STREAM, "level": 0, "score": 1.0, "timestamp": ts,
        "consolidated": false, "dormant": false, "in_progress": false
    }))
    .unwrap();
    c.source = session.map(|s| SourceTag {
        agent: "api".into(),
        session: Some(s.into()),
        channel: None,
    });
    c
}

async fn persist(state: &Arc<AppState>, chunk: Chunk) {
    let content = chunk.content.clone();
    let ts = i64::try_from(chunk.timestamp).unwrap();
    crate::handlers::ingest::persist_chunk(
        state, chunk, &content, "default", "default", STREAM, 0, ts, None, None,
    )
    .await
    .unwrap();
}

/// Session and timestamp of each fixture chunk: `a1`, `a2` in session A on
/// day 1, `b` in session B on day 2.
const FIXTURE: [(&str, &str, i64); 3] =
    [("a1", "A", DAY1), ("a2", "A", DAY1_LATER), ("b", "B", DAY2)];

async fn seed(state: &Arc<AppState>) {
    for (id, session, ts) in FIXTURE {
        let content = format!("groupfixture note {id}");
        persist(state, chunk(id, &content, Some(session), ts)).await;
    }
}

fn hit(id: &str, ts: i64) -> SearchResult {
    SearchResult {
        id: id.into(),
        content: format!("groupfixture note {id}"),
        score: 0.5,
        metadata: Some(json!({"timestamp": ts})),
        trace_info: None,
        signal_breakdown: None,
        content_type: None,
        content_type_source: None,
        evidence: None,
    }
}

fn hits(ids: &[&str]) -> Vec<SearchResult> {
    ids.iter()
        .map(|id| {
            let (_, _, ts) = FIXTURE.iter().find(|(f, _, _)| f == id).unwrap();
            hit(id, *ts)
        })
        .collect()
}

fn without_headers(text: &str) -> String {
    text.split_inclusive('\n')
        .filter(|line| !line.starts_with("## "))
        .collect()
}

fn headers(text: &str) -> Vec<String> {
    text.split_inclusive('\n')
        .filter(|line| line.starts_with("## "))
        .map(str::to_string)
        .collect()
}

// ── group_key ─────────────────────────────────────────────────────

#[test]
fn group_key_is_none_when_grouping_is_off() {
    let c = chunk("x", "x", Some("A"), DAY1);
    assert_eq!(group_key(SearchGroupBy::None, Some(&c), "2024-01-01"), None);
    assert_eq!(group_key(SearchGroupBy::None, None, ""), None);
}

#[test]
fn session_key_prefers_fragment_source_then_session_then_day() {
    let mut c = chunk("x", "x", Some("sess-1"), DAY1);
    let key = |c: Option<&Chunk>, ts| group_key(SearchGroupBy::Session, c, ts);
    assert_eq!(key(Some(&c), "2024-01-01"), Some("session: sess-1".into()));

    c.fragment = Some(FragmentMeta {
        source_id: "src-9".into(),
        byte_start: 0,
        byte_end: 1,
        turn_index: 0,
        role: None,
        conversation_date: None,
    });
    assert_eq!(key(Some(&c), "2024-01-01"), Some("session: src-9".into()));

    let mut bare = chunk("y", "y", None, DAY1);
    assert_eq!(
        key(Some(&bare), "event:2024-03-05"),
        Some("2024-03-05".into())
    );
    bare.source = Some(SourceTag::from_agent("api"));
    assert_eq!(key(Some(&bare), "2024-03-05"), Some("2024-03-05".into()));
    let blank = chunk("z", "z", Some("  "), DAY1);
    assert_eq!(key(Some(&blank), ""), Some("unknown date".into()));
    assert_eq!(key(None, "recorded:2024-02-29"), Some("2024-02-29".into()));
}

#[test]
fn day_key_reads_every_date_tag_form() {
    let day = |ts| group_key(SearchGroupBy::Day, None, ts);
    assert_eq!(day("2024-01-01"), Some("2024-01-01".into()));
    assert_eq!(day("event:2024-01-02"), Some("2024-01-02".into()));
    assert_eq!(day("observed:2024-01-03"), Some("2024-01-03".into()));
    assert_eq!(day("recorded:2024-01-04"), Some("2024-01-04".into()));
    assert_eq!(day("2024-01-05T10:00:00Z"), Some("2024-01-05".into()));
    assert_eq!(day(""), Some("unknown date".into()));
    assert_eq!(day("recorded:unknown"), Some("unknown date".into()));
    assert_eq!(day("last spring"), Some("unknown date".into()));
    // A session-bearing chunk still groups by day in day mode.
    let c = chunk("x", "x", Some("A"), DAY1);
    assert_eq!(
        group_key(SearchGroupBy::Day, Some(&c), "2024-01-01"),
        Some("2024-01-01".into())
    );
}

#[test]
fn aggregation_turns_grouping_off() {
    for mode in [
        SearchGroupBy::None,
        SearchGroupBy::Session,
        SearchGroupBy::Day,
    ] {
        assert_eq!(effective_mode(mode, true), SearchGroupBy::None);
        assert_eq!(effective_mode(mode, false), mode);
    }
}

// ── render_grouped ────────────────────────────────────────────────

#[tokio::test]
async fn none_is_exactly_render_lines() {
    let st = state(SearchGroupBy::None);
    seed(&st).await;
    let hs = hits(&["a1", "a2", "b"]);
    let preds = HashMap::new();
    assert_eq!(
        render_grouped(&st.store, &hs, &preds, SearchGroupBy::None),
        render_lines(&st.store, &hs, &preds)
    );
}

#[tokio::test]
async fn session_headers_precede_the_first_and_third_hit() {
    let st = state(SearchGroupBy::Session);
    seed(&st).await;
    let hs = hits(&["a1", "a2", "b"]);
    let preds = HashMap::new();
    let flat = render_lines(&st.store, &hs, &preds);
    let lines: Vec<&str> = flat.split_inclusive('\n').collect();
    let expected = format!(
        "## session: A\n{}{}## session: B\n{}",
        lines[0], lines[1], lines[2]
    );
    let grouped = render_grouped(&st.store, &hs, &preds, SearchGroupBy::Session);
    assert_eq!(grouped, expected);
    assert!(lines[2].starts_with("3. "), "{flat}");
}

#[tokio::test]
async fn day_headers_follow_the_date_tag() {
    let st = state(SearchGroupBy::Day);
    seed(&st).await;
    let hs = hits(&["a1", "a2", "b"]);
    let preds = HashMap::new();
    let flat = render_lines(&st.store, &hs, &preds);
    let lines: Vec<&str> = flat.split_inclusive('\n').collect();
    let expected = format!(
        "## 2024-01-01\n{}{}## 2024-01-02\n{}",
        lines[0], lines[1], lines[2]
    );
    assert_eq!(
        render_grouped(&st.store, &hs, &preds, SearchGroupBy::Day),
        expected
    );
}

#[tokio::test]
async fn non_contiguous_groups_repeat_their_header_without_reordering() {
    let st = state(SearchGroupBy::Session);
    seed(&st).await;
    let hs = hits(&["a1", "b", "a2"]);
    let preds = HashMap::new();
    let grouped = render_grouped(&st.store, &hs, &preds, SearchGroupBy::Session);
    assert_eq!(
        headers(&grouped),
        ["## session: A\n", "## session: B\n", "## session: A\n"]
    );
    assert_eq!(
        without_headers(&grouped),
        render_lines(&st.store, &hs, &preds)
    );
}

#[tokio::test]
async fn aggregation_mode_renders_no_headers() {
    let st = state(SearchGroupBy::Session);
    seed(&st).await;
    let hs = hits(&["a1", "a2", "b"]);
    let preds = HashMap::new();
    let mode = effective_mode(SearchGroupBy::Session, true);
    assert_eq!(
        render_grouped(&st.store, &hs, &preds, mode),
        render_lines(&st.store, &hs, &preds)
    );
}

// ── memory_search ─────────────────────────────────────────────────

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

/// The session headers expected for hits in `ids` order: one per change.
fn expected_session_headers(ids: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut previous = None;
    for id in ids {
        let (_, session, _) = FIXTURE.iter().find(|(f, _, _)| f == id).unwrap();
        if previous != Some(*session) {
            out.push(format!("## session: {session}\n"));
        }
        previous = Some(*session);
    }
    out
}

#[tokio::test]
async fn flag_off_text_is_the_grouped_text_without_headers() {
    let args = json!({"query": "groupfixture", "top_k": 5});
    let off = state(SearchGroupBy::None);
    seed(&off).await;
    let off_text = search(&off, args.clone()).await;
    let on = state(SearchGroupBy::Session);
    seed(&on).await;
    let on_text = search(&on, args).await;

    assert!(headers(&off_text).is_empty(), "{off_text}");
    let ids = hit_ids(&on_text);
    assert_eq!(ids.len(), 3, "{on_text}");
    assert_eq!(headers(&on_text), expected_session_headers(&ids));
    assert_eq!(without_headers(&on_text), off_text);
    assert!(off_text.starts_with("Found 3 relevant memories:\n\n"));
}

#[tokio::test]
async fn aggregation_query_gets_no_group_headers() {
    let args = json!({"query": "list all groupfixture"});
    let off = state(SearchGroupBy::None);
    seed(&off).await;
    let off_text = search(&off, args.clone()).await;
    let on = state(SearchGroupBy::Session);
    seed(&on).await;
    let on_text = search(&on, args).await;

    assert!(on_text.starts_with("ENUMERATION QUERY:"), "{on_text}");
    assert_eq!(hit_ids(&on_text).len(), 3, "{on_text}");
    assert!(headers(&on_text).is_empty(), "{on_text}");
    assert_eq!(on_text, off_text);
}
