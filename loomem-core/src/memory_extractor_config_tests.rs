use super::*;

struct NoCalls;

impl ExtractionChat for NoCalls {
    async fn chat(&self, _: &serde_json::Value) -> Result<ChatHttpReply> {
        panic!("invalid config must not call the extraction provider");
    }
}

#[tokio::test]
async fn invalid_extraction_budgets_fail_before_provider_call() {
    for (tokens, facts, completion_tokens, field) in [
        (0, 20, 2000, "max_transcript_tokens"),
        (usize::MAX, 20, 2000, "max_transcript_tokens"),
        (20_000, 0, 2000, "max_facts_per_chunk"),
        (20_000, 20, 0, "max_tokens"),
    ] {
        let config = KnowledgeExtractionConfig {
            max_transcript_tokens: tokens,
            max_facts_per_chunk: facts,
            max_tokens: completion_tokens,
            ..KnowledgeExtractionConfig::default()
        };
        let err = extract_knowledge_with(&NoCalls, &config, "user: test", "2026-10-13")
            .await
            .expect_err("invalid budget must fail");
        assert!(matches!(err, ExtractionError::InvalidConfig(_)));
        let (status, reason) = err.status_and_reason();
        assert_eq!(status, "config");
        assert!(reason.contains(field));
    }
}

#[test]
fn extraction_prompt_uses_configured_fact_cap_in_all_variants() {
    for cap in [1, 20, 40] {
        for topics in [
            None,
            Some(vec![]),
            Some(vec![ExtractionTopic {
                name: "milestone".to_string(),
                description: "Project milestone".to_string(),
                fact_type: None,
            }]),
        ] {
            let config = KnowledgeExtractionConfig {
                max_facts_per_chunk: cap,
                topics,
                ..KnowledgeExtractionConfig::default()
            };
            let prompt = build_extraction_prompt(&config, "2026-10-13");
            assert!(prompt.contains(&format!("Maximum {cap} facts per conversation")));
            assert!(!prompt.contains("{max_facts}"));
        }
    }
}

#[test]
fn valid_extraction_budgets_preserve_character_window_and_utf8() {
    let config = KnowledgeExtractionConfig::default();
    assert_eq!(
        config_validation::validated_chunk_size(&config).unwrap(),
        80_000
    );
    let text = "🦀ż".repeat(15);
    let chunks = split_with_overlap(&text, 20, 2);
    assert_eq!(chunks[0], "🦀ż".repeat(10));
    assert_eq!(chunks[1], "🦀ż".repeat(6));
}

/// /153 AC-2: no topics ⇒ the inline EXTRACTION_PROMPT body, prefixed with the
/// shared untrusted-data notice (the data/instruction boundary carried by every
/// prompt variant).
#[test]
fn build_prompt_no_topics_is_byte_identical() {
    let date = "2026-06-11";
    assert_eq!(
        build_extraction_prompt(&KnowledgeExtractionConfig::default(), date),
        prefix_untrusted_notice(
            &EXTRACTION_PROMPT
                .replace("{conversation_date}", date)
                .replace("{max_facts}", "20")
        )
    );
}

/// /153 AC-2: an explicitly empty topics list resolves to the same body.
#[test]
fn build_prompt_empty_topics_is_byte_identical() {
    let date = "2026-06-11";
    let mut config = KnowledgeExtractionConfig::default();
    config.topics = Some(vec![]);
    assert_eq!(
        build_extraction_prompt(&config, date),
        prefix_untrusted_notice(
            &EXTRACTION_PROMPT
                .replace("{conversation_date}", date)
                .replace("{max_facts}", "20")
        )
    );
}

#[test]
fn default_and_custom_prompts_never_substitute_session_date_for_unknown_event() {
    let default = KnowledgeExtractionConfig::default();
    let custom = KnowledgeExtractionConfig {
        topics: Some(vec![ExtractionTopic {
            name: "event".into(),
            description: "A personal event, with its source-supported date if known".into(),
            fact_type: None,
        }]),
        ..default.clone()
    };
    for config in [default, custom] {
        let prompt = build_extraction_prompt(&config, "2026-10-20");
        assert!(prompt.contains("explicit or resolvable temporal cue"));
        assert!(prompt.contains("never a substitute for an unknown event date"));
        assert!(prompt.contains("The conversation_date is: 2026-10-20"));
        assert!(!prompt.contains("else the conversation_date"));
    }
}

#[test]
fn unknown_reference_never_authorizes_current_date_relative_resolution() {
    let default = KnowledgeExtractionConfig::default();
    let custom = KnowledgeExtractionConfig {
        topics: Some(vec![ExtractionTopic {
            name: "event".into(),
            description: "A source-supported personal event".into(),
            fact_type: None,
        }]),
        ..default.clone()
    };
    for config in [default, custom] {
        let prompt = build_extraction_prompt(&config, "unknown");
        assert!(prompt.contains("The conversation_date is: unknown"));
        assert!(prompt.contains("relative dates require a known reference date"));
        assert!(prompt.contains("when it is unknown never resolve them using the current date"));
        assert!(prompt.contains("explicit absolute source dates are allowed"));
    }
}

/// Lowercase hex sha256 of a prompt string.
fn prompt_sha256(prompt: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    Sha256::digest(prompt.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn golden_topics_config() -> KnowledgeExtractionConfig {
    KnowledgeExtractionConfig {
        topics: Some(vec![ExtractionTopic {
            name: "milestone".to_string(),
            description: "Project milestone".to_string(),
            fact_type: None,
        }]),
        ..KnowledgeExtractionConfig::default()
    }
}

fn shipped_extraction_config() -> KnowledgeExtractionConfig {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../config.toml");
    let content = std::fs::read_to_string(path).unwrap();
    let table: toml::Table = toml::from_str(&content).unwrap();
    table["knowledge_extraction"].clone().try_into().unwrap()
}

/// Golden pin of the extraction system-prompt bytes, captured on origin/main
/// af9468bd. Any change to the prompt text, including a default-off option
/// leaking into it, changes these digests. Downstream prompt pins (a frozen
/// system-prompt sha in an evaluation proxy) rely on them.
#[test]
fn extraction_system_prompt_bytes_are_pinned() {
    let date = "2024-03-20";
    let cases = [
        (
            "default",
            build_extraction_prompt(&KnowledgeExtractionConfig::default(), date),
            "573876af7f38228ba4a8ed5a23ee3712818287ac78ea7bdb2254ceb93fb68a3f",
        ),
        (
            "topics",
            build_extraction_prompt(&golden_topics_config(), date),
            "80d5a6e5f56cb4fbd75a4573b7d07626ac297ec17681932e60d3f4e1f56af12e",
        ),
        (
            "shipped",
            build_extraction_prompt(&shipped_extraction_config(), date),
            "573876af7f38228ba4a8ed5a23ee3712818287ac78ea7bdb2254ceb93fb68a3f",
        ),
    ];
    for (name, prompt, expected) in cases {
        assert_eq!(prompt_sha256(&prompt), expected, "{name} prompt drifted");
    }
}

// ---- temporal grounding (`[knowledge_extraction] temporal_grounding`) ------

fn with_grounding(
    config: KnowledgeExtractionConfig,
    mode: TemporalGrounding,
) -> KnowledgeExtractionConfig {
    KnowledgeExtractionConfig {
        temporal_grounding: mode,
        ..config
    }
}

#[test]
fn temporal_grounding_defaults_to_off_when_key_is_missing() {
    assert_eq!(
        KnowledgeExtractionConfig::default().temporal_grounding,
        TemporalGrounding::Off
    );
    let legacy = "enabled = true\nmax_transcript_tokens = 20000\ndedup_cosine_threshold = 0.92\ncontradiction_check = true\ncontradiction_cosine_min = 0.5\n";
    let parsed: KnowledgeExtractionConfig = toml::from_str(legacy).unwrap();
    assert_eq!(parsed.temporal_grounding, TemporalGrounding::Off);
}

#[test]
fn temporal_grounding_parses_every_mode_and_rejects_unknown_values() {
    let base = "enabled = true\nmax_transcript_tokens = 20000\ndedup_cosine_threshold = 0.92\ncontradiction_check = true\ncontradiction_cosine_min = 0.5\n";
    for (value, mode) in [
        ("off", TemporalGrounding::Off),
        ("filter", TemporalGrounding::Filter),
        ("prompt_and_filter", TemporalGrounding::PromptAndFilter),
    ] {
        let text = format!("{base}temporal_grounding = \"{value}\"\n");
        let parsed: KnowledgeExtractionConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.temporal_grounding, mode);
    }
    let bogus = format!("{base}temporal_grounding = \"bogus\"\n");
    assert!(toml::from_str::<KnowledgeExtractionConfig>(&bogus).is_err());
}

#[test]
fn shipped_config_keeps_temporal_grounding_off() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../config.toml");
    let content = std::fs::read_to_string(path).unwrap();
    let table: toml::Table = toml::from_str(&content).unwrap();
    assert_eq!(
        table["knowledge_extraction"]["temporal_grounding"].as_str(),
        Some("off"),
        "the shipped config spells the key out"
    );
    assert_eq!(
        shipped_extraction_config().temporal_grounding,
        TemporalGrounding::Off
    );
}

/// `filter` never touches the prompt: both variants stay byte-identical to
/// `off`, whose bytes are pinned above.
#[test]
fn filter_mode_keeps_prompt_bytes_identical() {
    let date = "2024-03-20";
    for config in [KnowledgeExtractionConfig::default(), golden_topics_config()] {
        let off = build_extraction_prompt(&config, date);
        let filter = build_extraction_prompt(
            &with_grounding(config.clone(), TemporalGrounding::Filter),
            date,
        );
        assert_eq!(filter, off);
    }
}

/// `prompt_and_filter` prepends the grounding notice after the untrusted-data
/// notice, in both prompt variants, and changes nothing else.
#[test]
fn prompt_and_filter_prepends_notice_in_every_variant() {
    let date = "2024-03-20";
    let head = format!(
        "{}\n\nTEMPORAL GROUNDING: ",
        crate::sanitizer::UNTRUSTED_DATA_NOTICE
    );
    for config in [KnowledgeExtractionConfig::default(), golden_topics_config()] {
        let off = build_extraction_prompt(&config, date);
        let on = build_extraction_prompt(
            &with_grounding(config.clone(), TemporalGrounding::PromptAndFilter),
            date,
        );
        assert!(on.starts_with(&head), "{on}");
        let body = off
            .strip_prefix(&format!("{}\n\n", crate::sanitizer::UNTRUSTED_DATA_NOTICE))
            .unwrap();
        assert!(on.ends_with(&format!("\n\n{body}")));
        assert_eq!(
            on,
            prefix_untrusted_notice(&temporal_grounding::with_notice(
                TemporalGrounding::PromptAndFilter,
                body.to_string()
            ))
        );
    }
}

/// Stub transport: records every request body, returns one fixed reply.
struct Recording {
    reply: String,
    requests: std::sync::Mutex<Vec<serde_json::Value>>,
}

impl Recording {
    fn new(facts: serde_json::Value) -> Self {
        let content = serde_json::json!({ "facts": facts }).to_string();
        Self {
            reply: serde_json::json!({"choices": [{"message": {"content": content}}]}).to_string(),
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<serde_json::Value> {
        self.requests.lock().unwrap().clone()
    }
}

impl ExtractionChat for Recording {
    async fn chat(&self, request_body: &serde_json::Value) -> Result<ChatHttpReply> {
        self.requests.lock().unwrap().push(request_body.clone());
        Ok(ChatHttpReply {
            status: 200,
            body: self.reply.clone(),
        })
    }
}

const GROUNDING_SOURCE: &str =
    "user: I just got back from the synthetic festival. I met Anna yesterday.";

fn grounding_reply() -> serde_json::Value {
    let fact = |content: &str, cue: Option<&str>, confidence: f64| {
        serde_json::json!({
            "content": content, "fact_type": "event", "subject": null,
            "event_date": null, "event_date_context": cue,
            "attributed_to": "user", "confidence": confidence
        })
    };
    serde_json::json!([
        fact(
            "The user visited the synthetic festival three weeks ago.",
            None,
            0.9
        ),
        fact("The user likes festivals.", None, 0.4),
        fact("The user met Anna yesterday.", Some("yesterday"), 0.9),
    ])
}

async fn run_grounding(mode: TemporalGrounding) -> (Vec<String>, Vec<serde_json::Value>) {
    let config = with_grounding(KnowledgeExtractionConfig::default(), mode);
    let chat = Recording::new(grounding_reply());
    let outcome = extract_knowledge_with(&chat, &config, GROUNDING_SOURCE, "2024-03-20")
        .await
        .unwrap();
    let facts = outcome.facts.into_iter().map(|f| f.content).collect();
    (facts, chat.requests())
}

/// `off`: the request is exactly the pre-option request and admission is the
/// plain confidence filter (the invented "three weeks ago" fact is kept).
#[tokio::test]
async fn grounding_off_request_and_admission_are_unchanged() {
    let (facts, requests) = run_grounding(TemporalGrounding::Off).await;
    assert_eq!(
        facts,
        [
            "The user visited the synthetic festival three weeks ago.",
            "The user met Anna yesterday."
        ]
    );
    let config = KnowledgeExtractionConfig::default();
    let expected = build_extraction_request(
        &config.model,
        &build_extraction_prompt(&config, "2024-03-20"),
        0,
        GROUNDING_SOURCE,
        config.max_tokens,
    );
    assert_eq!(requests, [expected]);
}

/// `filter` / `prompt_and_filter`: the invented relative time is dropped, the
/// verbatim cue is kept; only `prompt_and_filter` sends the notice.
#[tokio::test]
async fn grounding_on_drops_invented_relative_time() {
    for (mode, sends_notice) in [
        (TemporalGrounding::Filter, false),
        (TemporalGrounding::PromptAndFilter, true),
    ] {
        let (facts, requests) = run_grounding(mode).await;
        assert_eq!(facts, ["The user met Anna yesterday."]);
        let system = requests[0]["messages"][0]["content"].as_str().unwrap();
        assert_eq!(system.contains("TEMPORAL GROUNDING"), sends_notice);
    }
}
