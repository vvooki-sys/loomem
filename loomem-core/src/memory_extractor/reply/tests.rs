use super::*;
use crate::memory_extractor::{extract_knowledge_with, ExtractionChat, KnowledgeExtractionConfig};
use serde_json::json;

const FACTS: &str =
    r#"{"facts":[{"content":"synthetic fact","fact_type":"fact","confidence":0.9}]}"#;

fn reply(choice: Value) -> ChatHttpReply {
    ChatHttpReply {
        status: 200,
        body: json!({"choices": [choice]}).to_string(),
    }
}

fn completion(content: Value, reason: Value) -> ChatHttpReply {
    reply(json!({"message": {"content": content}, "finish_reason": reason}))
}

#[test]
fn explicit_stop_accepts_complete_facts() {
    let facts = parse_extraction_reply(&completion(json!(FACTS), json!("stop")), 3).unwrap();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].content, "synthetic fact");
}

#[test]
fn missing_finish_reason_preserves_legacy_envelopes() {
    let facts = parse_extraction_reply(&reply(json!({"message": {"content": FACTS}})), 0).unwrap();
    assert_eq!(facts.len(), 1);
}

#[test]
fn length_rejects_valid_and_partial_json_without_leaking_content() {
    for content in [FACTS, "{\"facts\":[\"synthetic-private-payload"] {
        let failure = parse_extraction_reply(&completion(json!(content), json!("length")), 7)
            .expect_err("token-limited output must never admit facts");
        assert_eq!(failure.chunk_index, 7);
        assert_eq!(failure.status, Some(200));
        assert_eq!(
            failure.reason,
            "extraction completion truncated: output token limit reached"
        );
        assert!(!failure.reason.contains("synthetic"));
    }
}

#[test]
fn explicit_non_stop_metadata_fails_closed() {
    for reason in [
        Value::Null,
        json!("tool_calls"),
        json!("function_call"),
        json!("synthetic-private-status"),
        json!(12),
        json!({"private": "synthetic"}),
    ] {
        let failure = parse_extraction_reply(&completion(json!(FACTS), reason), 0).unwrap_err();
        assert_eq!(
            failure.reason,
            "extraction completion did not finish normally"
        );
    }
}

#[test]
fn filtered_or_refused_output_is_not_admitted() {
    let filtered = completion(Value::Null, json!("content_filter"));
    let refused = reply(json!({
        "message": {"content": FACTS, "refusal": "synthetic-private-refusal"},
        "finish_reason": "stop"
    }));
    for response in [filtered, refused] {
        let failure = parse_extraction_reply(&response, 0).unwrap_err();
        assert_eq!(failure.reason, "extraction completion refused by provider");
    }
}

#[test]
fn missing_choice_or_content_is_a_failure() {
    let empty = ChatHttpReply {
        status: 200,
        body: json!({"choices": []}).to_string(),
    };
    assert_eq!(
        parse_extraction_reply(&empty, 0).unwrap_err().reason,
        "extraction completion has no choice"
    );
    assert_eq!(
        parse_extraction_reply(&completion(Value::Null, json!("stop")), 0)
            .unwrap_err()
            .reason,
        "extraction completion has no content"
    );
}

struct TruncatedReply;

impl ExtractionChat for TruncatedReply {
    async fn chat(&self, _: &Value) -> anyhow::Result<ChatHttpReply> {
        Ok(completion(json!(FACTS), json!("length")))
    }
}

#[tokio::test]
async fn extraction_surfaces_truncation_instead_of_publishing_a_valid_prefix() {
    let failure = extract_knowledge_with(
        &TruncatedReply,
        &KnowledgeExtractionConfig::default(),
        "synthetic transcript",
        "2026-10-20",
    )
    .await
    .unwrap_err();
    assert_eq!(
        failure.status_and_reason(),
        (
            "200".to_string(),
            "extraction completion truncated: output token limit reached".to_string()
        )
    );
}
