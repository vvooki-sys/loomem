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
    let facts = parse_extraction_reply(&completion(json!(FACTS), json!("stop")), 3, false).unwrap();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].content, "synthetic fact");
}

#[test]
fn missing_finish_reason_preserves_legacy_envelopes() {
    let facts =
        parse_extraction_reply(&reply(json!({"message": {"content": FACTS}})), 0, false).unwrap();
    assert_eq!(facts.len(), 1);
}

#[test]
fn length_rejects_valid_and_partial_json_without_leaking_content() {
    for content in [FACTS, "{\"facts\":[\"synthetic-private-payload"] {
        let failure =
            parse_extraction_reply(&completion(json!(content), json!("length")), 7, false)
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
        let failure =
            parse_extraction_reply(&completion(json!(FACTS), reason), 0, false).unwrap_err();
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
        let failure = parse_extraction_reply(&response, 0, false).unwrap_err();
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
        parse_extraction_reply(&empty, 0, false).unwrap_err().reason,
        "extraction completion has no choice"
    );
    assert_eq!(
        parse_extraction_reply(&completion(Value::Null, json!("stop")), 0, false)
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

// ---- tolerate_trailing_json (holdout r4, 2026-11-25) ----------------------

/// The r4 shape: `finish_reason: stop`, a complete facts object, then stray
/// closers. `]}` is what the model actually appended; `}` is the single-brace
/// variant. Neither must be admitted on the strict path.
const STRAY_TAILS: [&str; 5] = ["]}", "}", "\n}\n", " ] } ", "}]}]}]}]"];

fn stray_tail_reply(tail: &str) -> ChatHttpReply {
    completion(json!(format!("{FACTS}{tail}")), json!("stop"))
}

#[test]
fn trailing_closers_are_rejected_without_the_flag() {
    for tail in STRAY_TAILS {
        let failure = parse_extraction_reply(&stray_tail_reply(tail), 2, false)
            .expect_err("strict parse must reject a tail");
        assert_eq!(failure.chunk_index, 2);
        assert_eq!(failure.status, Some(200));
        assert!(
            failure
                .reason
                .starts_with("facts parse failed: trailing characters at line"),
            "{}",
            failure.reason
        );
    }
}

#[test]
fn flag_admits_facts_before_a_short_tail_of_closers() {
    for tail in STRAY_TAILS {
        let facts = parse_extraction_reply(&stray_tail_reply(tail), 2, true)
            .unwrap_or_else(|f| panic!("tail {tail:?} must be tolerated: {}", f.reason));
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].content, "synthetic fact");
    }
}

#[test]
fn flag_tolerates_a_fenced_reply_with_a_stray_closer() {
    let fenced = completion(json!(format!("```json\n{FACTS}}}\n```")), json!("stop"));
    assert!(parse_extraction_reply(&fenced, 0, false).is_err());
    let facts = parse_extraction_reply(&fenced, 0, true).unwrap();
    assert_eq!(facts.len(), 1);
}

#[test]
fn flag_keeps_the_strict_error_for_text_or_long_tails() {
    // Short prose after the object, a tail over the cap, and a tail that
    // mixes closers with another character: none is "stray closers".
    let too_long = "}".repeat(MAX_TOLERATED_TAIL_CHARS + 1);
    for tail in [" ok", "\nDone.", too_long.as_str(), "}]}]}]}]}", "},"] {
        let strict = parse_extraction_reply(&stray_tail_reply(tail), 0, false).unwrap_err();
        let tolerant = parse_extraction_reply(&stray_tail_reply(tail), 0, true)
            .expect_err("only a short tail of closers may be dropped");
        assert_eq!(tolerant.reason, strict.reason, "tail {tail:?}");
        assert!(tolerant
            .reason
            .starts_with("facts parse failed: trailing characters"));
    }
}

#[test]
fn flag_does_not_repair_other_syntax_errors() {
    for content in [
        "{\"facts\":[{\"content\":\"synthetic fact\",\"fact_type\":\"fact\",\"confidence\":0.9}]",
        "{\"facts\":[{\"content\":\"synthetic fact\",\"fact_type\":\"fact\",\"confidence\":0.9}}]}",
        "{\"facts\": \"synthetic-not-a-list\"}}",
        "synthetic prose, no JSON at all",
    ] {
        let strict = parse_extraction_reply(&completion(json!(content), json!("stop")), 0, false)
            .unwrap_err();
        let tolerant = parse_extraction_reply(&completion(json!(content), json!("stop")), 0, true)
            .expect_err("damage inside the object is never salvaged");
        assert_eq!(tolerant.reason, strict.reason, "{content}");
    }
}

#[test]
fn truncation_still_wins_over_the_flag() {
    let failure = parse_extraction_reply(&stray_tail_reply("]}"), 0, true)
        .map(|_| ())
        .err();
    assert!(failure.is_none(), "stop + tail is admitted");
    let truncated = completion(json!(format!("{FACTS}]}}")), json!("length"));
    let failure = parse_extraction_reply(&truncated, 0, true).unwrap_err();
    assert_eq!(failure.reason, TRUNCATED_REASON);
}

#[test]
fn tolerable_tail_rule_is_bounded_and_closers_only() {
    assert!(is_tolerable_tail("]}"));
    assert!(is_tolerable_tail("\n } ] \n"));
    assert!(is_tolerable_tail(&"}".repeat(MAX_TOLERATED_TAIL_CHARS)));
    assert!(!is_tolerable_tail(""));
    assert!(!is_tolerable_tail("   "));
    assert!(!is_tolerable_tail(
        &"}".repeat(MAX_TOLERATED_TAIL_CHARS + 1)
    ));
    assert!(!is_tolerable_tail("})"));
    assert!(!is_tolerable_tail("}x"));
}

struct StrayTailReply;

impl ExtractionChat for StrayTailReply {
    async fn chat(&self, _: &Value) -> anyhow::Result<ChatHttpReply> {
        Ok(stray_tail_reply("]}"))
    }
}

#[tokio::test]
async fn extraction_run_loses_the_chunk_without_the_flag_and_keeps_it_with() {
    let strict = KnowledgeExtractionConfig::default();
    let err = extract_knowledge_with(
        &StrayTailReply,
        &strict,
        "synthetic transcript",
        "2026-11-25",
    )
    .await
    .unwrap_err();
    let (status, reason) = err.status_and_reason();
    assert_eq!(status, "200");
    assert!(reason.starts_with("facts parse failed: trailing characters"));

    let tolerant = KnowledgeExtractionConfig {
        tolerate_trailing_json: true,
        ..KnowledgeExtractionConfig::default()
    };
    let outcome = extract_knowledge_with(
        &StrayTailReply,
        &tolerant,
        "synthetic transcript",
        "2026-11-25",
    )
    .await
    .unwrap();
    assert_eq!(outcome.facts.len(), 1);
    assert!(outcome.failures.is_empty());
    assert_eq!(outcome.chunks_completed, 1);
}

#[test]
fn tolerate_trailing_json_defaults_off_and_parses_from_toml() {
    assert!(!KnowledgeExtractionConfig::default().tolerate_trailing_json);
    let base = "enabled = true\nmax_transcript_tokens = 20000\ndedup_cosine_threshold = 0.92\ncontradiction_check = true\ncontradiction_cosine_min = 0.5\n";
    let parsed: KnowledgeExtractionConfig = toml::from_str(base).unwrap();
    assert!(!parsed.tolerate_trailing_json);
    let parsed: KnowledgeExtractionConfig =
        toml::from_str(&format!("{base}tolerate_trailing_json = true\n")).unwrap();
    assert!(parsed.tolerate_trailing_json);

    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../config.toml");
    let table: toml::Table = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let shipped: KnowledgeExtractionConfig =
        table["knowledge_extraction"].clone().try_into().unwrap();
    assert!(
        !shipped.tolerate_trailing_json,
        "ships off, byte-identical path"
    );
}
