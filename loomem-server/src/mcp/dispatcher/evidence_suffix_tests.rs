//! `memory_search` text lines carry the bracketed evidence tag only when
//! `[search] evidence_fields` is on; off, the line is byte-identical.
use super::tool_search;
use crate::AppState;
use loomem_core::fragments::FragmentMeta;
use loomem_core::storage::Chunk;
use serde_json::json;
use std::sync::Arc;

const STREAM: &str = "evidence-suffix";

fn state(evidence_fields: bool) -> Arc<AppState> {
    let (_, original) = crate::tests::make_test_app();
    let mut config = original.config.clone();
    config.search.evidence_fields = evidence_fields;
    crate::tests::make_test_app_cfg(config).1
}

fn chunk(id: &str, content: &str) -> Chunk {
    serde_json::from_value(json!({
        "id": id, "content": content,
        "stream": STREAM, "level": 0, "score": 1.0, "timestamp": 1_704_067_200,
        "consolidated": false, "dormant": false, "in_progress": false
    }))
    .unwrap()
}

async fn persist(state: &Arc<AppState>, chunk: Chunk) {
    let content = chunk.content.clone();
    crate::handlers::ingest::persist_chunk(
        state,
        chunk,
        &content,
        "default",
        "default",
        STREAM,
        0,
        1_704_067_200,
        None,
        None,
    )
    .await
    .unwrap();
}

/// A fragment chunk whose source exists (lineage is validated on store).
async fn seed(state: &Arc<AppState>) {
    persist(state, chunk("source-9", "whole transcript")).await;
    let mut chunk = chunk("frag-1", "suffixfixture quarterly budget approved");
    chunk.source_ids = Some(vec!["source-9".into()]);
    chunk.fragment = Some(FragmentMeta {
        source_id: "source-9".into(),
        byte_start: 0,
        byte_end: 39,
        turn_index: 0,
        role: Some("user".into()),
        conversation_date: Some("2024-01-01".into()),
    });
    persist(state, chunk).await;
}

async fn search_line(state: &Arc<AppState>) -> String {
    let result = tool_search(state, json!({"query": "suffixfixture", "top_k": 5}), STREAM)
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    result.content[0]
        .text
        .lines()
        .find(|line| line.contains("suffixfixture"))
        .unwrap_or_else(|| panic!("missing hit: {}", result.content[0].text))
        .to_string()
}

#[tokio::test]
async fn suffix_is_absent_by_default_and_present_before_score_when_enabled() {
    let off = state(false);
    seed(&off).await;
    let plain = search_line(&off).await;
    assert!(!plain.contains("[author:"), "{plain}");
    assert!(plain.ends_with("(id: frag-1)"), "{plain}");

    let on = state(true);
    seed(&on).await;
    let tagged = search_line(&on).await;
    let tag = " [author: user; conversation: 2024-01-01; source: source-9#0-39]";
    assert!(tagged.contains(&format!("{tag} (score: ")), "{tagged}");
    assert!(tagged.ends_with("(id: frag-1)"), "{tagged}");
    assert_eq!(tagged.replacen(tag, "", 1), plain);
}
