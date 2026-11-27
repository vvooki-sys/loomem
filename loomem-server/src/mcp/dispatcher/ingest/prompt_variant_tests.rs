//! `[knowledge_extraction] prompt_variant` through `memory_ingest`, with an
//! injected chat transport and real storage (keyless fixture). The stub reply
//! is synthetic: these tests prove selection and that a fact's cause,
//! direction and value survive parsing and persistence, not that a model
//! writes such facts.
use super::fixtures::*;

use loomem_core::memory_extractor::{build_extraction_prompt, ExtractionPromptVariant};
use serde_json::{json, Value};
use std::sync::Arc;

use super::*;
use crate::AppState;

const SOURCE: &str = "user: We pushed the launch to May because the security audit failed. \
My rent went up from $1,200 to $1,350. The vintage guitar was appraised at about $4,500.";

const KEPT: [&str; 3] = [
    "The team postponed the launch to May because the security audit failed.",
    "The user's rent increased from $1,200 to $1,350.",
    "The user's vintage guitar was appraised at about $4,500.",
];

fn relations_reply() -> Chat {
    Chat::facts(
        KEPT.iter()
            .map(|content| {
                let mut row = fact(None);
                row["content"] = json!(content);
                row
            })
            .collect(),
    )
}

fn system_prompt(chat: &Chat) -> String {
    let requests = chat.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    requests[0]["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn ingest(variant: ExtractionPromptVariant, stream: &str) -> (Arc<AppState>, Chat, Value) {
    let (_, state) = fixture_with_prompt_variant(variant);
    let chat = relations_reply();
    let result = tool_ingest_with(&chat, &state, args(SOURCE), stream, None)
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    let outcome =
        result.structured_content.as_ref().unwrap()["extraction_receipt"]["outcome"].clone();
    (state, chat, outcome)
}

fn stored_facts(state: &AppState, stream: &str) -> Vec<String> {
    let mut facts: Vec<String> = chunks(state, stream)
        .into_iter()
        .filter(|row| kind(row) == "fact")
        .map(|row| row.content)
        .collect();
    facts.sort_unstable();
    facts
}

#[tokio::test]
async fn default_variant_sends_the_unchanged_prompt() {
    let (state, chat, outcome) = ingest(ExtractionPromptVariant::Default, "variant-default").await;
    let system = system_prompt(&chat);
    assert!(!system.contains("RELATIONS AND VALUES"));
    let mut config = state.config.knowledge_extraction.clone();
    config.prompt_variant = ExtractionPromptVariant::Default;
    assert_eq!(system, build_extraction_prompt(&config, "2024-03-20"));
    assert_eq!(outcome["accepted_facts"], 3);
}

#[tokio::test]
async fn relations_variant_sends_notice_and_stored_facts_keep_relations() {
    let (state, chat, outcome) =
        ingest(ExtractionPromptVariant::Relations, "variant-relations").await;
    let system = system_prompt(&chat);
    assert!(system.contains("\n\nRELATIONS AND VALUES: Keep what connects and quantifies a fact."));
    assert_eq!(
        system,
        build_extraction_prompt(&state.config.knowledge_extraction, "2024-03-20")
    );
    assert_eq!(outcome["accepted_facts"], 3);
    let mut expected: Vec<String> = KEPT.iter().map(|s| s.to_string()).collect();
    expected.sort_unstable();
    assert_eq!(stored_facts(&state, "variant-relations"), expected);
}

#[tokio::test]
async fn relations_v2_variant_sends_v2_notice_and_stored_facts_keep_relations() {
    let (state, chat, outcome) =
        ingest(ExtractionPromptVariant::RelationsV2, "variant-relations-v2").await;
    let system = system_prompt(&chat);
    assert!(system.contains("\n\nRELATIONS AND VALUES: Keep what connects and quantifies a fact."));
    assert!(system.contains("output format stays the same. SCOPE: "));
    assert!(system.contains(" SPEAKER: "));
    assert!(system.contains("as the ATTRIBUTION rule says.\n\n"));
    assert_eq!(
        system,
        build_extraction_prompt(&state.config.knowledge_extraction, "2024-03-20")
    );
    assert_eq!(outcome["accepted_facts"], 3);
    let mut expected: Vec<String> = KEPT.iter().map(|s| s.to_string()).collect();
    expected.sort_unstable();
    assert_eq!(stored_facts(&state, "variant-relations-v2"), expected);
}
