//! Full-text snapshot of the `memory_search` output after the line renderer
//! moved into `search_format`: header, one line per hit (plain, `[UPDATED]`,
//! trust tier `b`) and footer, scores masked. Plus the `[trust:?]` sentinel
//! for a hit whose chunk is not in the store.
use super::search_format::{render_line, render_lines};
use super::tool_search;
use crate::handlers::types::SearchResult;
use crate::AppState;
use loomem_core::storage::Chunk;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

const STREAM: &str = "search-format";
const RECORDED: i64 = 1_704_067_200; // 2024-01-01

fn chunk(id: &str, content: &str) -> Chunk {
    serde_json::from_value(json!({
        "id": id, "content": content,
        "stream": STREAM, "level": 0, "score": 1.0, "timestamp": RECORDED,
        "consolidated": false, "dormant": false, "in_progress": false
    }))
    .unwrap()
}

async fn persist(state: &Arc<AppState>, chunk: Chunk) {
    let content = chunk.content.clone();
    crate::handlers::ingest::persist_chunk(
        state, chunk, &content, "default", "default", STREAM, 0, RECORDED, None, None,
    )
    .await
    .unwrap();
}

/// Three live hits: a plain note, a successor (`supersedes_id` set, its
/// predecessor stored but no longer latest) and a low-trust note.
async fn seed(state: &Arc<AppState>) {
    persist(state, chunk("plain", "snapfixture plain note")).await;
    let mut old = chunk("old", "snapfixture stale note");
    old.is_latest = false;
    persist(state, old).await;
    let mut updated = chunk("updated", "snapfixture updated note");
    updated.supersedes_id = Some("old".into());
    persist(state, updated).await;
    let mut low = chunk("lowtrust", "snapfixture guarded note");
    low.trust_level = Some("b".into());
    persist(state, low).await;
}

/// The line each fixture id must render to, score masked.
fn expected_line(index: usize, id: &str) -> String {
    let (tag, content, note) = match id {
        "plain" => ("[trust:a1 ×1.00]", "snapfixture plain note", ""),
        "updated" => (
            "[trust:a1 ×1.00]",
            "snapfixture updated note",
            " [UPDATED — supersedes older version]",
        ),
        "lowtrust" => ("[trust:b ×0.80]", "snapfixture guarded note", ""),
        other => panic!("unexpected hit {other}"),
    };
    format!("{index}. [2024-01-01] {tag} {content}{note} (score: #) (id: {id})\n")
}

fn mask_scores(text: &str) -> String {
    regex::Regex::new(r"\(score: [0-9.]+\)")
        .unwrap()
        .replace_all(text, "(score: #)")
        .into_owned()
}

fn hit_ids(text: &str) -> Vec<String> {
    regex::Regex::new(r"\(id: ([^)]+)\)")
        .unwrap()
        .captures_iter(text)
        .map(|c| c[1].to_string())
        .collect()
}

#[tokio::test]
async fn tool_search_text_is_header_rendered_lines_and_footer() {
    let (_, state) = crate::tests::make_test_app();
    seed(&state).await;
    let result = tool_search(&state, json!({"query": "snapfixture", "top_k": 5}), STREAM)
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    let text = mask_scores(&result.content[0].text);

    // Ranking among equal-length ties is not part of this contract; the
    // snapshot is rebuilt in the observed id order and compared whole.
    let ids = hit_ids(&text);
    assert_eq!(ids.len(), 3, "{text}");
    let mut expected = "Found 3 relevant memories:\n\n".to_string();
    for (i, id) in ids.iter().enumerate() {
        expected.push_str(&expected_line(i + 1, id));
    }
    expected.push_str("\nNote: When facts have dates, ALWAYS prefer the most recent version. Items marked [UPDATED] supersede older versions.");
    assert_eq!(text, expected);
}

fn hit(id: &str, timestamp: Option<i64>, content_type: Option<&str>) -> SearchResult {
    SearchResult {
        id: id.into(),
        content: format!("ghost {id}"),
        score: 0.5,
        metadata: timestamp.map(|ts| json!({"timestamp": ts})),
        trace_info: None,
        signal_breakdown: None,
        content_type: content_type.map(str::to_string),
        content_type_source: None,
        evidence: None,
    }
}

#[test]
fn render_line_marks_a_missing_chunk_with_the_trust_sentinel() {
    let (_, state) = crate::tests::make_test_app();
    assert_eq!(
        render_line(&state.store, 0, &hit("g1", Some(RECORDED), None), None),
        "1. [2024-01-01] [trust:?] ghost g1 (score: 0.50) (id: g1)\n"
    );
    assert_eq!(
        render_line(&state.store, 4, &hit("g2", None, Some("case_study")), None),
        "5. [] [case_study] [trust:?] ghost g2 (score: 0.50) (id: g2)\n"
    );
    let both = [hit("g1", Some(RECORDED), None), hit("g2", None, None)];
    assert_eq!(
        render_lines(&state.store, &both, &HashMap::new()),
        "1. [2024-01-01] [trust:?] ghost g1 (score: 0.50) (id: g1)\n\
         2. [] [trust:?] ghost g2 (score: 0.50) (id: g2)\n"
    );
    assert_eq!(render_lines(&state.store, &[], &HashMap::new()), "");
}
