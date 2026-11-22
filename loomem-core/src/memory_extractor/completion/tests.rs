use super::*;
use crate::memory_extractor::{
    extract_knowledge_with, ChatHttpReply, ExtractionChat, KnowledgeExtractionConfig,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Mutex;

struct ScriptedChat(Mutex<VecDeque<ChatHttpReply>>);

impl ScriptedChat {
    fn new(replies: Vec<ChatHttpReply>) -> Self {
        Self(Mutex::new(replies.into()))
    }

    fn remaining(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

impl ExtractionChat for ScriptedChat {
    async fn chat(&self, _request_body: &Value) -> anyhow::Result<ChatHttpReply> {
        Ok(self.0.lock().unwrap().pop_front().expect("unexpected call"))
    }
}

fn reply(reason: &str, contents: &[&str]) -> ChatHttpReply {
    let facts: Vec<Value> = contents
        .iter()
        .map(|content| json!({"content":content,"fact_type":"fact","confidence":0.9}))
        .collect();
    ChatHttpReply {
        status: 200,
        body: json!({"choices":[{"finish_reason":reason,
            "message":{"content":json!({"facts":facts}).to_string()}}]})
        .to_string(),
    }
}

const FOUR_LINES: &str = "l1 aaaa\nl2 bbbb\nl3 cccc\nl4 dddd";

fn two_chunk_config(cap: usize) -> KnowledgeExtractionConfig {
    KnowledgeExtractionConfig {
        input_chunk_tokens: 4,
        max_facts_per_chunk: cap,
        ..KnowledgeExtractionConfig::default()
    }
}

fn contents(outcome: &ExtractionOutcome) -> Vec<&str> {
    outcome
        .facts
        .iter()
        .map(|fact| fact.content.as_str())
        .collect()
}

/// Every chunk returning exactly its cap sets the flag without any loss.
#[tokio::test]
async fn exact_per_chunk_cap_has_no_observed_loss() {
    let chat = ScriptedChat::new(vec![reply("stop", &["A"]), reply("stop", &["B"])]);
    let outcome = extract_knowledge_with(&chat, &two_chunk_config(1), FOUR_LINES, "2026-10-26")
        .await
        .unwrap();
    assert_eq!(contents(&outcome), ["A", "B"]);
    assert_eq!(outcome.input_chunks_total, 2);
    assert_eq!(outcome.input_chunks_processed, 2);
    assert!(outcome.fact_limit_reached);
    assert_eq!(outcome.discarded_facts, 0);
    assert_eq!(chat.remaining(), 0);
}

/// A chunk's excess over its own cap is counted; the other chunk's facts
/// are untouched.
#[tokio::test]
async fn excess_facts_on_one_input_chunk_are_counted() {
    let chat = ScriptedChat::new(vec![reply("stop", &["A"]), reply("stop", &["B", "C", "D"])]);
    let outcome = extract_knowledge_with(&chat, &two_chunk_config(2), FOUR_LINES, "2026-10-26")
        .await
        .unwrap();
    assert_eq!(contents(&outcome), ["A", "B", "C"]);
    assert_eq!(outcome.input_chunks_processed, outcome.input_chunks_total);
    assert!(outcome.fact_limit_reached);
    assert_eq!(outcome.discarded_facts, 1);
    assert_eq!(chat.remaining(), 0);
}

/// The cap is per chunk: a chunk that fills it does not stop the chunks
/// after it, and each later chunk gets its own cap.
#[tokio::test]
async fn cap_on_one_chunk_does_not_stop_later_chunks() {
    let chat = ScriptedChat::new(vec![
        reply("stop", &["A", "B", "X"]),
        reply("stop", &["C", "D"]),
    ]);
    let outcome = extract_knowledge_with(&chat, &two_chunk_config(2), FOUR_LINES, "2026-10-26")
        .await
        .unwrap();
    assert_eq!(contents(&outcome), ["A", "B", "C", "D"]);
    assert_eq!(outcome.input_chunks_total, 2);
    assert_eq!(outcome.input_chunks_processed, 2);
    assert_eq!(outcome.chunks_attempted, 2);
    assert!(outcome.fact_limit_reached);
    assert_eq!(outcome.discarded_facts, 1, "only the first chunk's excess");
    assert_eq!(chat.remaining(), 0, "every planned chunk is extracted");
}

#[tokio::test]
async fn split_calls_and_failed_chunks_do_not_inflate_input_coverage() {
    let config = KnowledgeExtractionConfig {
        split_on_truncation: true,
        ..two_chunk_config(10)
    };
    let chat = ScriptedChat::new(vec![
        reply("length", &["not admitted"]),
        reply("stop", &["A"]),
        reply("stop", &["B"]),
        ChatHttpReply {
            status: 429,
            body: "quota exhausted".into(),
        },
    ]);
    let outcome = extract_knowledge_with(&chat, &config, FOUR_LINES, "2026-10-26")
        .await
        .unwrap();
    assert_eq!(contents(&outcome), ["A", "B"]);
    assert_eq!(outcome.input_chunks_total, 2);
    assert_eq!(outcome.input_chunks_processed, 2);
    assert_eq!(outcome.chunks_attempted, 4);
    assert_eq!(outcome.chunks_completed, 2);
    assert_eq!(outcome.chunks_truncated, 1);
    assert_eq!(outcome.failures.len(), 1);
    assert_eq!(outcome.failures[0].chunk_index, 1);
    assert!(!outcome.fact_limit_reached);
    assert_eq!(outcome.discarded_facts, 0);
    assert_eq!(chat.remaining(), 0);
}

#[tokio::test]
async fn below_cap_is_distinct_from_reaching_it_with_zero_discarded_facts() {
    let chat = ScriptedChat::new(vec![reply("stop", &[]), reply("stop", &["A"])]);
    let outcome = extract_knowledge_with(&chat, &two_chunk_config(2), FOUR_LINES, "2026-10-26")
        .await
        .unwrap();
    assert_eq!(contents(&outcome), ["A"]);
    assert_eq!(outcome.input_chunks_total, 2);
    assert_eq!(outcome.input_chunks_processed, 2);
    assert!(!outcome.fact_limit_reached);
    assert_eq!(outcome.discarded_facts, 0);
}
