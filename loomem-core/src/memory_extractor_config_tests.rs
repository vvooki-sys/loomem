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
        (20_000, 0, 2000, "max_facts_per_transcript"),
        (20_000, 20, 0, "max_tokens"),
    ] {
        let config = KnowledgeExtractionConfig {
            max_transcript_tokens: tokens,
            max_facts_per_transcript: facts,
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
                max_facts_per_transcript: cap,
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
