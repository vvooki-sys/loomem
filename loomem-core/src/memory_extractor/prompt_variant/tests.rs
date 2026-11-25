use super::{ExtractionPromptVariant, NOTICE};
use crate::memory_extractor::{
    build_extraction_prompt, extract_knowledge_with, ChatHttpReply, ExtractionChat,
    ExtractionTopic, KnowledgeExtractionConfig, TemporalGrounding,
};
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Mutex;

const DATE: &str = "2024-03-20";

/// The minimal `[knowledge_extraction]` keys every config carries.
const BASE: &str = "enabled = true\nmax_transcript_tokens = 20000\ndedup_cosine_threshold = 0.92\ncontradiction_check = true\ncontradiction_cosine_min = 0.5\n";

fn sha256_hex(text: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    Sha256::digest(text.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn with_variant(
    config: KnowledgeExtractionConfig,
    variant: ExtractionPromptVariant,
) -> KnowledgeExtractionConfig {
    KnowledgeExtractionConfig {
        prompt_variant: variant,
        ..config
    }
}

fn topics_config() -> KnowledgeExtractionConfig {
    KnowledgeExtractionConfig {
        topics: Some(vec![ExtractionTopic {
            name: "milestone".to_string(),
            description: "Project milestone".to_string(),
            fact_type: None,
        }]),
        ..KnowledgeExtractionConfig::default()
    }
}

fn shipped_config() -> KnowledgeExtractionConfig {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../config.toml");
    let content = std::fs::read_to_string(path).unwrap();
    let table: toml::Table = toml::from_str(&content).unwrap();
    table["knowledge_extraction"].clone().try_into().unwrap()
}

fn untrusted_head() -> String {
    format!("{}\n\n", crate::sanitizer::UNTRUSTED_DATA_NOTICE)
}

/// The default variant sends the exact system-prompt bytes of origin/main
/// 9c88f5bb (same digests as `extraction_system_prompt_bytes_are_pinned`).
/// The paid evaluation harness pins this prompt's sha256; a default variant
/// that leaked into the prompt would change these digests.
#[test]
fn default_variant_prompt_bytes_are_pinned() {
    let default = with_variant(
        KnowledgeExtractionConfig::default(),
        ExtractionPromptVariant::Default,
    );
    let topics = with_variant(topics_config(), ExtractionPromptVariant::Default);
    let shipped = shipped_config();
    assert_eq!(shipped.prompt_variant, ExtractionPromptVariant::Default);
    for (name, config, expected) in [
        (
            "default",
            default,
            "573876af7f38228ba4a8ed5a23ee3712818287ac78ea7bdb2254ceb93fb68a3f",
        ),
        (
            "topics",
            topics,
            "80d5a6e5f56cb4fbd75a4573b7d07626ac297ec17681932e60d3f4e1f56af12e",
        ),
        (
            "shipped",
            shipped,
            "573876af7f38228ba4a8ed5a23ee3712818287ac78ea7bdb2254ceb93fb68a3f",
        ),
    ] {
        let prompt = build_extraction_prompt(&config, DATE);
        assert_eq!(sha256_hex(&prompt), expected, "{name} prompt drifted");
        assert!(!prompt.contains("RELATIONS AND VALUES"), "{name}");
    }
}

#[test]
fn prompt_variant_defaults_when_key_is_missing() {
    assert_eq!(
        KnowledgeExtractionConfig::default().prompt_variant,
        ExtractionPromptVariant::Default
    );
    let parsed: KnowledgeExtractionConfig = toml::from_str(BASE).unwrap();
    assert_eq!(parsed.prompt_variant, ExtractionPromptVariant::Default);
}

#[test]
fn prompt_variant_parses_every_value_and_rejects_unknown_values() {
    for (value, variant) in [
        ("default", ExtractionPromptVariant::Default),
        ("relations", ExtractionPromptVariant::Relations),
    ] {
        let text = format!("{BASE}prompt_variant = \"{value}\"\n");
        let parsed: KnowledgeExtractionConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.prompt_variant, variant);
    }
    let bogus = format!("{BASE}prompt_variant = \"bogus\"\n");
    assert!(toml::from_str::<KnowledgeExtractionConfig>(&bogus).is_err());
}

#[test]
fn shipped_config_spells_out_the_default_variant() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../config.toml");
    let content = std::fs::read_to_string(path).unwrap();
    let table: toml::Table = toml::from_str(&content).unwrap();
    assert_eq!(
        table["knowledge_extraction"]["prompt_variant"].as_str(),
        Some("default")
    );
}

/// The notice names the three relations the variant exists for.
#[test]
fn relations_notice_covers_cause_direction_and_values() {
    for needle in [
        "RELATIONS AND VALUES:",
        "CAUSE:",
        "because",
        "due to",
        "DIRECTION:",
        "from what to what",
        "increased or decreased",
        "started or stopped",
        "Never invert",
        "VALUES:",
        "amount",
        "valuation",
        "currency",
        "date",
        "with the entity it belongs to",
        "output format stays the same",
    ] {
        assert!(NOTICE.contains(needle), "notice lacks {needle:?}");
    }
}

/// `relations` prepends the notice right after the untrusted-data notice and
/// changes nothing else: the rest, including the JSON contract, is the
/// default prompt byte for byte. Holds for the built-in and the topic prompt.
#[test]
fn relations_prepends_notice_and_keeps_the_rest_identical() {
    for config in [KnowledgeExtractionConfig::default(), topics_config()] {
        let default = build_extraction_prompt(&config, DATE);
        let relations = build_extraction_prompt(
            &with_variant(config.clone(), ExtractionPromptVariant::Relations),
            DATE,
        );
        let body = default.strip_prefix(&untrusted_head()).unwrap();
        assert_eq!(relations, format!("{}{NOTICE}\n\n{body}", untrusted_head()));
        assert!(relations.contains("Return ONLY valid JSON"));
    }
}

/// With both notices on, the order is untrusted-data, temporal grounding,
/// relations, then the prompt body.
#[test]
fn relations_composes_with_temporal_grounding() {
    let grounded = KnowledgeExtractionConfig {
        temporal_grounding: TemporalGrounding::PromptAndFilter,
        ..KnowledgeExtractionConfig::default()
    };
    let without = build_extraction_prompt(&grounded, DATE);
    let with = build_extraction_prompt(
        &with_variant(grounded, ExtractionPromptVariant::Relations),
        DATE,
    );
    let body = build_extraction_prompt(&KnowledgeExtractionConfig::default(), DATE);
    let body = body.strip_prefix(&untrusted_head()).unwrap();
    let grounding_head = without.strip_suffix(body).unwrap();
    assert!(grounding_head.contains("TEMPORAL GROUNDING:"));
    assert_eq!(with, format!("{grounding_head}{NOTICE}\n\n{body}"));
}

/// Stub transport: records every request body, returns one fixed reply.
struct Recording {
    reply: String,
    requests: Mutex<Vec<Value>>,
}

impl ExtractionChat for Recording {
    async fn chat(&self, request_body: &Value) -> Result<ChatHttpReply> {
        self.requests.lock().unwrap().push(request_body.clone());
        Ok(ChatHttpReply {
            status: 200,
            body: self.reply.clone(),
        })
    }
}

const SOURCE: &str = "user: We pushed the launch to May because the security audit failed. \
My rent went up from $1,200 to $1,350. The vintage guitar was appraised at about $4,500.";

/// Synthetic facts that carry a cause, a direction of change and a value.
const KEPT: [&str; 3] = [
    "The team postponed the launch to May because the security audit failed.",
    "The user's rent increased from $1,200 to $1,350.",
    "The user's vintage guitar was appraised at about $4,500.",
];

fn relations_reply() -> Recording {
    let facts: Vec<Value> = KEPT
        .iter()
        .map(|content| {
            json!({"content": content, "fact_type": "fact", "subject": null,
                "event_date": null, "event_date_context": null,
                "attributed_to": "user", "confidence": 0.9})
        })
        .collect();
    let content = json!({ "facts": facts }).to_string();
    Recording {
        reply: json!({"choices": [{"message": {"content": content}}]}).to_string(),
        requests: Mutex::new(Vec::new()),
    }
}

/// Plumbing, not model behaviour: with `relations` selected, the request
/// carries the notice, and a reply whose facts keep a cause, a direction and
/// a value comes out of the extraction path with that content unchanged.
#[tokio::test]
async fn relations_variant_is_sent_and_facts_keep_cause_direction_value() {
    for (variant, sends_notice) in [
        (ExtractionPromptVariant::Default, false),
        (ExtractionPromptVariant::Relations, true),
    ] {
        let config = with_variant(KnowledgeExtractionConfig::default(), variant);
        let chat = relations_reply();
        let outcome = extract_knowledge_with(&chat, &config, SOURCE, DATE)
            .await
            .unwrap();
        let contents: Vec<&str> = outcome.facts.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(contents, KEPT);
        let requests = chat.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let system = requests[0]["messages"][0]["content"].as_str().unwrap();
        assert_eq!(system, build_extraction_prompt(&config, DATE));
        assert_eq!(system.contains(NOTICE), sends_notice);
    }
}
