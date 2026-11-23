//! `[knowledge_extraction] temporal_grounding` through `memory_ingest`, with
//! an injected chat transport and real storage (keyless fixture).
use super::fixtures::*;

use loomem_core::memory_extractor::{build_extraction_prompt, TemporalGrounding};
use serde_json::{json, Value};
use std::sync::Arc;

use super::*;
use crate::AppState;

const SOURCE: &str = "user: I just got back from the synthetic festival.";

/// The extraction reply invents a relative duration the source never states.
fn invented_relative_time() -> Chat {
    let mut invented = fact(None);
    invented["content"] = json!("The user visited the synthetic festival three weeks ago.");
    Chat::facts(vec![invented])
}

fn system_prompt(chat: &Chat) -> String {
    let requests = chat.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    requests[0]["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The prompt every mode but `prompt_and_filter` must send: the one built
/// from the fixture's extraction config with grounding off.
fn off_prompt(state: &AppState) -> String {
    let mut config = state.config.knowledge_extraction.clone();
    config.temporal_grounding = TemporalGrounding::Off;
    build_extraction_prompt(&config, "2024-03-20")
}

/// The receipt's 9 outcome keys before temporal grounding existed.
const OFF_OUTCOME_KEYS: [&str; 9] = [
    "accepted_facts",
    "calls_attempted",
    "calls_completed",
    "calls_truncated",
    "discarded_facts",
    "fact_limit_reached",
    "failed_attempts",
    "input_chunks_processed",
    "input_chunks_total",
];

async fn ingest(mode: TemporalGrounding, stream: &str) -> (Arc<AppState>, Chat, Value) {
    let (_, state) = fixture_with_grounding(mode);
    let chat = invented_relative_time();
    let result = tool_ingest_with(&chat, &state, args(SOURCE), stream, None)
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    let outcome =
        result.structured_content.as_ref().unwrap()["extraction_receipt"]["outcome"].clone();
    (state, chat, outcome)
}

fn outcome_keys(outcome: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = outcome
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

#[tokio::test]
async fn invented_relative_time_kept_when_grounding_off() {
    let (state, chat, outcome) = ingest(TemporalGrounding::Off, "grounding-off").await;
    let records = chunks(&state, "grounding-off");
    assert_eq!(records.len(), 2);
    let source = records.iter().find(|row| kind(row) == "source").unwrap();
    let derived = records.iter().find(|row| kind(row) == "fact").unwrap();
    assert_link(derived, source);
    assert!(derived.content.contains("three weeks ago"));
    let system = system_prompt(&chat);
    assert!(!system.contains("TEMPORAL GROUNDING"));
    assert_eq!(system, off_prompt(&state));
    // Off: the receipt carries no grounding count, only the original keys.
    assert_eq!(outcome_keys(&outcome), OFF_OUTCOME_KEYS);
    assert_eq!(outcome["accepted_facts"], 1);
}

#[tokio::test]
async fn invented_relative_time_rejected_when_grounding_filter() {
    let (state, chat, outcome) = ingest(TemporalGrounding::Filter, "grounding-filter").await;
    let records = chunks(&state, "grounding-filter");
    assert_eq!(records.len(), 1);
    assert_eq!(kind(&records[0]), "source");
    assert!(records[0].content.contains("just got back"));
    assert_eq!(system_prompt(&chat), off_prompt(&state));
    // Every fact was rejected by grounding: the receipt shows the count, and
    // it is not reported as discarded or failed.
    assert_eq!(outcome["grounding_rejected_facts"], 1);
    assert_eq!(outcome["accepted_facts"], 0);
    assert_eq!(outcome["discarded_facts"], 0);
    assert_eq!(outcome["failed_attempts"], 0);
}

#[tokio::test]
async fn prompt_and_filter_sends_notice() {
    let (state, chat, outcome) =
        ingest(TemporalGrounding::PromptAndFilter, "grounding-prompt").await;
    let records = chunks(&state, "grounding-prompt");
    assert_eq!(records.len(), 1);
    assert_eq!(kind(&records[0]), "source");
    let system = system_prompt(&chat);
    assert!(system.contains("\n\nTEMPORAL GROUNDING: Preserve the source's temporal wording."));
    assert_ne!(system, off_prompt(&state));
    assert_eq!(outcome["grounding_rejected_facts"], 1);
}
