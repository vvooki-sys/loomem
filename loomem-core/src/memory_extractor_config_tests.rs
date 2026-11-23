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
