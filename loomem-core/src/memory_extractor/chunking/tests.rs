use super::*;
use crate::memory_extractor::{
    build_extraction_prompt, extract_knowledge_with, ChatHttpReply, ExtractionError,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Mutex;

/// Stub transport: pops one scripted reply per call and records every
/// request body, so tests can assert call counts, order and chunk texts.
struct ScriptedChat {
    replies: Mutex<VecDeque<anyhow::Result<ChatHttpReply>>>,
    requests: Mutex<Vec<Value>>,
}

impl ScriptedChat {
    fn new(replies: Vec<ChatHttpReply>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().map(Ok).collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    /// Transcript text each call carried, with the boundary markers stripped.
    fn chunk_texts(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|body| {
                let user = body["messages"][1]["content"].as_str().unwrap();
                let (_, rest) = user.split_once("]\n").unwrap();
                rest.strip_suffix("\n[/CHUNK]").unwrap().to_string()
            })
            .collect()
    }
}

impl ExtractionChat for ScriptedChat {
    async fn chat(&self, request_body: &Value) -> anyhow::Result<ChatHttpReply> {
        self.requests.lock().unwrap().push(request_body.clone());
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("more extraction calls than scripted replies")
    }
}

fn reply(finish_reason: &str, facts: &[&str]) -> ChatHttpReply {
    let facts: Vec<Value> = facts
        .iter()
        .map(|c| json!({"content": c, "fact_type": "fact", "confidence": 0.9}))
        .collect();
    ChatHttpReply {
        status: 200,
        body: json!({"choices": [{
            "finish_reason": finish_reason,
            "message": {"content": json!({"facts": facts}).to_string()}
        }]})
        .to_string(),
    }
}

fn complete(facts: &[&str]) -> ChatHttpReply {
    reply("stop", facts)
}

fn truncated() -> ChatHttpReply {
    reply("length", &["prefix-must-not-be-admitted"])
}

fn contents(facts: &[ExtractedFact]) -> Vec<&str> {
    facts.iter().map(|f| f.content.as_str()).collect()
}

const FOUR_LINES: &str = "l1 aaaa\nl2 bbbb\nl3 cccc\nl4 dddd";

// ---- default behaviour is unchanged ---------------------------------------

/// Default config: one call per transcript whose request body is exactly the
/// pre-existing shape (model, prompt, marker-wrapped chunk 0, max_tokens 2000).
#[tokio::test]
async fn default_config_makes_one_call_with_unchanged_request_body() {
    let config = KnowledgeExtractionConfig::default();
    let chat = ScriptedChat::new(vec![complete(&["A"])]);
    let outcome = extract_knowledge_with(&chat, &config, FOUR_LINES, "2026-10-21")
        .await
        .unwrap();
    let requests = chat.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0],
        build_extraction_request(
            &config.model,
            &build_extraction_prompt(&config, "2026-10-21"),
            0,
            FOUR_LINES,
            2000
        )
    );
    assert_eq!(contents(&outcome.facts), ["A"]);
    assert_eq!(
        (
            outcome.chunks_attempted,
            outcome.chunks_completed,
            outcome.chunks_truncated
        ),
        (1, 1, 0)
    );
}

/// Split disabled (default): a truncated reply is rejected after exactly one
/// call — no retry, no admitted facts, reported as all-chunks-failed.
#[tokio::test]
async fn split_disabled_makes_zero_retries() {
    let chat = ScriptedChat::new(vec![truncated()]);
    let err = extract_knowledge_with(
        &chat,
        &KnowledgeExtractionConfig::default(),
        FOUR_LINES,
        "2026-10-21",
    )
    .await
    .unwrap_err();
    assert_eq!(chat.requests().len(), 1);
    match err {
        ExtractionError::AllChunksFailed {
            chunks_attempted,
            failures,
        } => {
            assert_eq!(chunks_attempted, 1);
            assert_eq!(failures.len(), 1);
            assert!(failures[0].is_truncated());
        }
        other => panic!("unexpected error: {other}"),
    }
}

/// Split enabled but nothing truncates: call count equals chunk count.
#[tokio::test]
async fn split_enabled_without_truncation_adds_no_calls() {
    let config = KnowledgeExtractionConfig {
        split_on_truncation: true,
        ..KnowledgeExtractionConfig::default()
    };
    let chat = ScriptedChat::new(vec![complete(&["A"])]);
    let outcome = extract_knowledge_with(&chat, &config, FOUR_LINES, "2026-10-21")
        .await
        .unwrap();
    assert_eq!(chat.requests().len(), 1);
    assert_eq!(contents(&outcome.facts), ["A"]);
    assert!(outcome.failures.is_empty());
}

// ---- input chunking -------------------------------------------------------

/// `input_chunk_tokens` splits at line boundaries into N calls; facts are
/// concatenated in transcript order and the cap applies per chunk.
#[tokio::test]
async fn input_chunking_makes_line_aligned_calls_and_merges_under_cap() {
    // 4 tokens ≈ 16 chars: two 8-char lines ("l1 aaaa\n") fit per chunk.
    let config = KnowledgeExtractionConfig {
        input_chunk_tokens: 4,
        max_facts_per_chunk: 3,
        ..KnowledgeExtractionConfig::default()
    };
    let chat = ScriptedChat::new(vec![complete(&["A", "B"]), complete(&["C", "D"])]);
    let outcome = extract_knowledge_with(&chat, &config, FOUR_LINES, "2026-10-21")
        .await
        .unwrap();
    assert_eq!(
        chat.chunk_texts(),
        ["l1 aaaa\nl2 bbbb\n", "l3 cccc\nl4 dddd"]
    );
    assert_eq!(contents(&outcome.facts), ["A", "B", "C", "D"]);
    assert_eq!(outcome.chunks_attempted, 2);
    assert_eq!(outcome.chunks_completed, 2);
    assert!(outcome.failures.is_empty());
}

/// `input_chunk_tokens` never exceeds `max_transcript_tokens` (clamp), and a
/// transcript within budget is one chunk.
#[tokio::test]
async fn input_chunking_clamps_to_transcript_budget() {
    let config = KnowledgeExtractionConfig {
        input_chunk_tokens: usize::MAX,
        max_transcript_tokens: 4,
        ..KnowledgeExtractionConfig::default()
    };
    let chat = ScriptedChat::new(vec![complete(&[]), complete(&[])]);
    extract_knowledge_with(&chat, &config, FOUR_LINES, "2026-10-21")
        .await
        .unwrap();
    assert_eq!(chat.requests().len(), 2);
}

// ---- split on truncation --------------------------------------------------

/// First reply truncated → two half calls; facts admitted only from complete
/// replies; a half that truncates again is split once more until the per-chunk
/// budget is exhausted, then reported as a truncation failure.
#[tokio::test]
async fn split_on_truncation_retries_halves_and_admits_only_complete_replies() {
    let config = KnowledgeExtractionConfig {
        split_on_truncation: true,
        ..KnowledgeExtractionConfig::default()
    };
    let chat = ScriptedChat::new(vec![
        truncated(),      // whole chunk
        complete(&["A"]), // left half
        truncated(),      // right half
        complete(&["B"]), // right-left quarter
        truncated(),      // right-right quarter: budget exhausted, no split
    ]);
    let outcome = extract_knowledge_with(&chat, &config, FOUR_LINES, "2026-10-21")
        .await
        .unwrap();
    assert_eq!(
        chat.chunk_texts(),
        [
            FOUR_LINES,
            "l1 aaaa\nl2 bbbb",
            "l3 cccc\nl4 dddd",
            "l3 cccc",
            "l4 dddd",
        ]
    );
    assert_eq!(chat.requests().len(), 1 + MAX_SPLIT_CALLS_PER_CHUNK);
    assert_eq!(contents(&outcome.facts), ["A", "B"]);
    assert_eq!(outcome.failures.len(), 1);
    assert!(outcome.failures[0].is_truncated());
    assert_eq!(outcome.failures[0].chunk_index, 0);
    assert_eq!(
        (
            outcome.chunks_attempted,
            outcome.chunks_completed,
            outcome.chunks_truncated
        ),
        (5, 2, 3)
    );
}

/// Budget bound: once the left half spent the remaining two calls on its
/// quarters, a truncated right half gets no further split.
#[tokio::test]
async fn split_budget_is_bounded_per_original_chunk() {
    let config = KnowledgeExtractionConfig {
        split_on_truncation: true,
        ..KnowledgeExtractionConfig::default()
    };
    let chat = ScriptedChat::new(vec![
        truncated(),      // whole
        truncated(),      // left half → quarters
        complete(&["A"]), // left-left
        complete(&["B"]), // left-right
        truncated(),      // right half: budget exhausted
    ]);
    let outcome = extract_knowledge_with(&chat, &config, FOUR_LINES, "2026-10-21")
        .await
        .unwrap();
    assert_eq!(chat.requests().len(), 1 + MAX_SPLIT_CALLS_PER_CHUNK);
    assert_eq!(contents(&outcome.facts), ["A", "B"]);
    assert_eq!(outcome.failures.len(), 1);
    assert_eq!(outcome.chunks_truncated, 3);
}

/// A single-line chunk cannot be split: one call, reported as truncated.
#[tokio::test]
async fn split_on_truncation_needs_a_line_boundary() {
    let config = KnowledgeExtractionConfig {
        split_on_truncation: true,
        ..KnowledgeExtractionConfig::default()
    };
    let chat = ScriptedChat::new(vec![truncated()]);
    let err = extract_knowledge_with(&chat, &config, "single line", "2026-10-21")
        .await
        .unwrap_err();
    assert_eq!(chat.requests().len(), 1);
    assert!(matches!(err, ExtractionError::AllChunksFailed { .. }));
}

/// Depth guard independent of the call budget.
#[test]
fn split_for_retry_respects_depth_and_budget_guards() {
    let on = KnowledgeExtractionConfig {
        split_on_truncation: true,
        ..KnowledgeExtractionConfig::default()
    };
    let off = KnowledgeExtractionConfig::default();
    assert!(split_for_retry(&on, FOUR_LINES, 0, MAX_SPLIT_CALLS_PER_CHUNK).is_some());
    assert!(split_for_retry(&on, FOUR_LINES, MAX_SPLIT_DEPTH, MAX_SPLIT_CALLS_PER_CHUNK).is_none());
    assert!(split_for_retry(&on, FOUR_LINES, 0, 1).is_none());
    assert!(split_for_retry(&off, FOUR_LINES, 0, MAX_SPLIT_CALLS_PER_CHUNK).is_none());
}

// ---- pure helpers ---------------------------------------------------------

#[test]
fn line_aligned_chunks_never_cut_lines_and_reproduce_text() {
    let text = "ab\ncdef\ng\nhijklmnop\nq";
    let chunks = line_aligned_chunks(text, 5);
    assert_eq!(chunks, ["ab\n", "cdef\n", "g\n", "hijklmnop\n", "q"]);
    assert_eq!(chunks.concat(), text);
    assert_eq!(line_aligned_chunks(text, 100), [text]);
    assert_eq!(line_aligned_chunks("", 5), [""]);
}

#[test]
fn line_aligned_chunks_count_characters_not_bytes() {
    let text = "żż\nżż\n";
    assert_eq!(line_aligned_chunks(text, 3), ["żż\n", "żż\n"]);
    assert_eq!(line_aligned_chunks(text, 6), [text]);
}

#[test]
fn split_at_line_boundary_prefers_midpoint_and_non_blank_halves() {
    assert_eq!(
        split_at_line_boundary(FOUR_LINES),
        Some((
            "l1 aaaa\nl2 bbbb".to_string(),
            "l3 cccc\nl4 dddd".to_string()
        ))
    );
    assert_eq!(split_at_line_boundary("single line"), None);
    assert_eq!(split_at_line_boundary("\n\nonly\n\n"), None);
    assert_eq!(
        split_at_line_boundary("a\n\n"),
        None,
        "a boundary that leaves a blank half is skipped"
    );
    assert_eq!(
        split_at_line_boundary("żżżż\nx"),
        Some(("żżżż".to_string(), "x".to_string()))
    );
}

// ---- config compatibility -------------------------------------------------

/// A TOML without the new keys parses to the same values as before (and as
/// `Default`); explicit keys are honored.
#[test]
fn config_without_new_keys_keeps_legacy_values() {
    let legacy = r#"
        enabled = true
        model = "gpt-4.1-mini"
        max_transcript_tokens = 20000
        dedup_cosine_threshold = 0.92
        contradiction_check = true
        contradiction_cosine_min = 0.5
        max_facts_per_transcript = 20
    "#;
    let parsed: KnowledgeExtractionConfig = toml::from_str(legacy).unwrap();
    let defaults = KnowledgeExtractionConfig::default();
    assert_eq!(parsed.max_tokens, 2000);
    assert_eq!(parsed.input_chunk_tokens, defaults.input_chunk_tokens);
    assert_eq!(parsed.split_on_truncation, defaults.split_on_truncation);
    assert_eq!(parsed.input_chunk_tokens, 0);
    assert!(!parsed.split_on_truncation);

    let explicit = format!("{legacy}\ninput_chunk_tokens = 4000\nsplit_on_truncation = true\n");
    let parsed: KnowledgeExtractionConfig = toml::from_str(&explicit).unwrap();
    assert_eq!(parsed.input_chunk_tokens, 4000);
    assert!(parsed.split_on_truncation);
}

/// The shipped config.toml carries the chunked-extraction defaults decided
/// on 2026-11-22 (owner decision, no A/B; the validation pilot measures
/// them) under the per-chunk cap key.
#[test]
fn shipped_config_toml_enables_chunked_extraction() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../config.toml");
    let content = std::fs::read_to_string(path).unwrap();
    let table: toml::Table = toml::from_str(&content).unwrap();
    assert!(
        table["knowledge_extraction"]
            .get("max_facts_per_transcript")
            .is_none(),
        "the shipped config uses the per-chunk key"
    );
    let section: KnowledgeExtractionConfig =
        table["knowledge_extraction"].clone().try_into().unwrap();
    assert_eq!(section.max_tokens, 16384);
    assert_eq!(section.input_chunk_tokens, 4000);
    assert!(section.split_on_truncation);
    assert_eq!(section.max_facts_per_chunk, 20);
}

// ---- chunked defaults with a per-chunk cap --------------------------------

/// A transcript under `input_chunk_tokens` is one chunk: the request the
/// provider receives is byte-identical to the legacy single-call pipeline
/// (`input_chunk_tokens = 0`), so the new defaults change nothing for short
/// transcripts.
#[tokio::test]
async fn short_transcript_request_is_byte_identical_to_legacy_single_call() {
    let legacy = KnowledgeExtractionConfig {
        model: "gpt-6-luna".to_string(),
        input_chunk_tokens: 0,
        split_on_truncation: false,
        ..KnowledgeExtractionConfig::default()
    };
    let chunked = KnowledgeExtractionConfig {
        input_chunk_tokens: 4000,
        split_on_truncation: true,
        ..legacy.clone()
    };
    // ~2 000 chars ≈ 500 tokens, well under the 4 000-token chunk.
    let transcript =
        "user: We ship the beta on Friday.\nassistant: Noted, Friday it is.\n".repeat(30);
    let legacy_chat = ScriptedChat::new(vec![complete(&["A"])]);
    let chunked_chat = ScriptedChat::new(vec![complete(&["A"])]);
    extract_knowledge_with(&legacy_chat, &legacy, &transcript, "2026-11-22")
        .await
        .unwrap();
    extract_knowledge_with(&chunked_chat, &chunked, &transcript, "2026-11-22")
        .await
        .unwrap();
    assert_eq!(legacy_chat.requests().len(), 1);
    assert_eq!(chunked_chat.requests().len(), 1);
    let legacy_bytes = serde_json::to_vec(&legacy_chat.requests()[0]).unwrap();
    let chunked_bytes = serde_json::to_vec(&chunked_chat.requests()[0]).unwrap();
    assert_eq!(chunked_bytes, legacy_bytes);
}

/// A long transcript is split into chunks and every chunk keeps its own
/// cap: three chunks returning 20 facts each yield 60, not the former
/// transcript-wide 20.
#[tokio::test]
async fn long_transcript_keeps_facts_beyond_the_former_transcript_cap() {
    // 4 tokens ≈ 16 chars: two 8-char lines per chunk, six lines → 3 chunks.
    let config = KnowledgeExtractionConfig {
        input_chunk_tokens: 4,
        split_on_truncation: true,
        max_facts_per_chunk: 20,
        ..KnowledgeExtractionConfig::default()
    };
    let transcript = "l1 aaaa\nl2 bbbb\nl3 cccc\nl4 dddd\nl5 eeee\nl6 ffff";
    let twenty =
        |prefix: &str| -> Vec<String> { (1..=20).map(|n| format!("{prefix}{n}")).collect() };
    let replies = ["a", "b", "c"]
        .iter()
        .map(|p| {
            let facts = twenty(p);
            complete(&facts.iter().map(String::as_str).collect::<Vec<_>>())
        })
        .collect();
    let chat = ScriptedChat::new(replies);
    let outcome = extract_knowledge_with(&chat, &config, transcript, "2026-11-22")
        .await
        .unwrap();
    assert_eq!(chat.requests().len(), 3);
    assert_eq!(outcome.input_chunks_total, 3);
    assert_eq!(outcome.input_chunks_processed, 3);
    assert_eq!(outcome.facts.len(), 60);
    assert_eq!(outcome.facts[0].content, "a1");
    assert_eq!(outcome.facts[20].content, "b1");
    assert_eq!(outcome.facts[59].content, "c20");
    assert_eq!(outcome.discarded_facts, 0);
    assert!(
        outcome.fact_limit_reached,
        "each chunk returned exactly its cap"
    );
    assert!(outcome.failures.is_empty());
}

/// An operator's pre-2026-11-22 config (`max_facts_per_transcript = 20`,
/// `input_chunk_tokens = 0`) parses through the alias and behaves exactly as
/// before: one call per transcript, the prompt names the same cap, and a
/// reply over the cap is truncated to 20 with the excess counted.
#[tokio::test]
async fn legacy_transcript_cap_config_keeps_its_behavior() {
    let legacy = r#"
        enabled = true
        model = "gpt-4.1-mini"
        max_transcript_tokens = 20000
        dedup_cosine_threshold = 0.92
        contradiction_check = true
        contradiction_cosine_min = 0.5
        max_facts_per_transcript = 20
        max_tokens = 2000
        input_chunk_tokens = 0
        split_on_truncation = false
    "#;
    let config: KnowledgeExtractionConfig = toml::from_str(legacy).unwrap();
    assert_eq!(config.max_facts_per_chunk, 20);
    assert!(build_extraction_prompt(&config, "2026-11-22")
        .contains("Maximum 20 facts per conversation"));

    let facts: Vec<String> = (1..=25).map(|n| format!("f{n}")).collect();
    let chat = ScriptedChat::new(vec![complete(
        &facts.iter().map(String::as_str).collect::<Vec<_>>(),
    )]);
    let outcome = extract_knowledge_with(&chat, &config, FOUR_LINES, "2026-11-22")
        .await
        .unwrap();
    assert_eq!(chat.requests().len(), 1, "one call per transcript");
    assert_eq!(chat.requests()[0]["max_tokens"], 2000);
    assert_eq!(outcome.facts.len(), 20);
    assert_eq!(outcome.facts[19].content, "f20");
    assert_eq!(outcome.discarded_facts, 5);
    assert!(outcome.fact_limit_reached);
    assert_eq!(outcome.input_chunks_total, 1);
    assert_eq!(outcome.input_chunks_processed, 1);
}

/// Both spellings name one field: a config carrying both is rejected rather
/// than silently picking one.
#[test]
fn both_fact_cap_keys_are_rejected() {
    let both = r#"
        enabled = true
        max_transcript_tokens = 20000
        dedup_cosine_threshold = 0.92
        contradiction_check = true
        contradiction_cosine_min = 0.5
        max_facts_per_transcript = 20
        max_facts_per_chunk = 20
    "#;
    assert!(toml::from_str::<KnowledgeExtractionConfig>(both).is_err());
}
