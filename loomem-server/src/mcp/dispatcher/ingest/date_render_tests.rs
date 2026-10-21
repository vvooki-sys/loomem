//! Exercise real MCP search/context formatting over the same persisted fixtures.
use super::super::{tool_ingest_with, IngestPersistence, RealPersistence};
use super::{fact, fixture, Chat};
use crate::mcp::dispatcher::{tool_context, tool_search};
use loomem_core::storage::Chunk;
use serde_json::json;
use std::sync::Arc;

const STREAM: &str = "phase15";

async fn ingest_pair(
    state: &Arc<crate::AppState>,
    name: &str,
    conversation_date: Option<&str>,
    event_date: Option<&str>,
) {
    let mut derived = fact(event_date);
    derived["content"] = json!(format!("{name}_FACT datefixture personal statement"));
    let chat = Chat::facts(vec![derived]);
    tool_ingest_with(
        &chat,
        state,
        json!({"content":format!("{name}_SOURCE datefixture original statement"),
            "conversation_date":conversation_date}),
        STREAM,
        None,
    )
    .await
    .unwrap();
    assert_eq!(chat.calls(), 1);
}

async fn seed(state: &Arc<crate::AppState>) {
    ingest_pair(state, "OBSERVED", Some("2024-03-20"), None).await;
    ingest_pair(state, "EVENT", Some("2024-03-20"), Some("2020-02-03")).await;
    ingest_pair(state, "RECORDED", None, None).await;
    let legacy: Chunk = serde_json::from_value(json!({
        "id":"legacy-date-fixture", "content":"LEGACY datefixture unchanged statement",
        "stream":STREAM, "level":0, "score":1.0, "timestamp":1704067200,
        "consolidated":false, "dormant":false, "in_progress":false,
    }))
    .unwrap();
    RealPersistence.persist(state, legacy).await.unwrap();
}

fn expected() -> [(&'static str, &'static str); 7] {
    [
        ("OBSERVED_SOURCE", "observed:2024-03-20"),
        ("OBSERVED_FACT", "observed:2024-03-20"),
        ("EVENT_SOURCE", "observed:2024-03-20"),
        ("EVENT_FACT", "event:2020-02-03"),
        ("RECORDED_SOURCE", "recorded:2026-10-19"),
        ("RECORDED_FACT", "recorded:2026-10-19"),
        ("LEGACY", "2024-01-01"),
    ]
}

#[tokio::test]
async fn mcp_search_and_both_context_sections_render_real_source_date_provenance() {
    let (_, state) = fixture(20000);
    seed(&state).await;
    let search = tool_search(&state, json!({"query":"datefixture", "top_k":20}), STREAM)
        .await
        .unwrap();
    assert_ne!(search.is_error, Some(true));
    let search = &search.content[0].text;
    let context = tool_context(
        &state,
        json!({"query":"datefixture", "budget_tokens":8000,
        "sections":["relevant", "recent"]}),
        STREAM,
    )
    .await
    .unwrap();
    assert_ne!(context.is_error, Some(true));
    let (_, sections) = context.content[0]
        .text
        .split_once("## Relevant Memories\n\n")
        .unwrap();
    let (relevant, recent) = sections.split_once("## Recent Context\n\n").unwrap();
    for (name, date) in expected() {
        let search_line = search
            .lines()
            .find(|line| line.contains(name))
            .unwrap_or_else(|| panic!("missing {name}: {search}"));
        let (_, header) = search_line.split_once(". ").unwrap();
        assert!(header.starts_with(&format!("[{date}]")), "{search_line}");
        for section in [relevant, recent] {
            let line = section
                .lines()
                .find(|line| line.contains(name))
                .unwrap_or_else(|| panic!("missing {name}: {section}"));
            assert!(
                line.starts_with(&format!("- [{date}] {name} datefixture")),
                "{line}"
            );
        }
    }
}

#[tokio::test]
async fn context_relevant_rejects_tombstoned_source_family_even_with_stale_bm25() {
    let (_, state) = fixture(20000);
    ingest_pair(&state, "DELETED", Some("2024-03-20"), None).await;
    let source = state
        .store
        .get_all_chunks()
        .unwrap()
        .into_iter()
        .find(|chunk| loomem_core::storage::is_ingest_source(chunk))
        .unwrap();
    let deleted = state
        .store
        .tombstone_ingest_source(&source.id, 1_792_432_801)
        .unwrap()
        .unwrap();
    assert_eq!(deleted.len(), 2);
    let stale = state
        .tantivy
        .lock()
        .await
        .search_with_stream("datefixture", STREAM, 20)
        .unwrap();
    assert_eq!(stale.len(), 2, "fixture must retain stale index entries");
    let context = tool_context(
        &state,
        json!({"query":"datefixture", "budget_tokens":8000,
        "sections":["relevant", "recent"]}),
        STREAM,
    )
    .await
    .unwrap();
    assert_ne!(context.is_error, Some(true));
    assert!(!context.content[0].text.contains("DELETED_"));
}
