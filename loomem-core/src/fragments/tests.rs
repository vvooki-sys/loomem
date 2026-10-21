use super::*;
use crate::config::Config;
use crate::storage::ProvenanceRole;

fn source(text: &str) -> CanonicalSource {
    CanonicalSource {
        id: "src-1".to_string(),
        text: text.to_string(),
        conversation_date: Some("2026-10-21".to_string()),
    }
}

fn cfg(max_fragment_tokens: usize) -> FragmentsConfig {
    FragmentsConfig {
        enabled: true,
        max_fragment_tokens,
        ..FragmentsConfig::default()
    }
}

fn segment(text: &str, max_tokens: usize) -> Vec<SourceFragment> {
    segment_source(&source(text), &cfg(max_tokens), &ApproxTokenCounter).expect("segment")
}

/// Independent re-check of the tiling/slice/id invariants plus the budget.
fn assert_well_formed(src: &CanonicalSource, fragments: &[SourceFragment], max_tokens: usize) {
    verify_invariants(src, fragments).expect("invariants");
    for f in fragments {
        assert!(src.text.is_char_boundary(f.byte_start));
        assert!(src.text.is_char_boundary(f.byte_end));
        let chars = f.text.chars().count();
        assert!(
            ApproxTokenCounter.count(&f.text).unwrap() <= max_tokens || chars == 1,
            "fragment {} over budget: {} chars",
            f.id,
            chars
        );
    }
}

fn roles(fragments: &[SourceFragment]) -> Vec<Option<&str>> {
    fragments.iter().map(|f| f.role.as_deref()).collect()
}

#[test]
fn empty_text_yields_no_fragments() {
    assert!(segment("", 400).is_empty());
}

#[test]
fn zero_budget_is_rejected() {
    let err = segment_source(&source("hi"), &cfg(0), &ApproxTokenCounter).expect_err("zero");
    assert!(matches!(err, FragmentError::InvalidConfig));
}

#[test]
fn no_markers_is_a_single_unlabelled_turn() {
    let fragments = segment("Just some prose.\nNo roles here.", 400);
    assert_eq!(fragments.len(), 1);
    assert_eq!(fragments[0].role, None);
    assert_eq!(fragments[0].turn_index, 0);
    assert_eq!(fragments[0].id, "src-1:f0");
    assert_eq!(
        fragments[0].conversation_date.as_deref(),
        Some("2026-10-21")
    );
}

#[test]
fn colon_markers_split_turns_and_keep_trailing_whitespace() {
    let text = "user: hello\n\nassistant: hi there\nsystem: note\n";
    let fragments = segment(text, 400);
    assert_eq!(
        roles(&fragments),
        vec![Some("user"), Some("assistant"), Some("system")]
    );
    assert_eq!(fragments[0].text, "user: hello\n\n");
    assert_eq!(fragments[1].text, "assistant: hi there\n");
    assert_eq!(fragments[2].text, "system: note\n");
    assert_eq!(
        fragments.iter().map(|f| f.turn_index).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
}

#[test]
fn roles_are_normalised() {
    let text = "HUMAN: a\nAI: b\nUser: c\nAssistant: d\nSYSTEM: e";
    let fragments = segment(text, 400);
    assert_eq!(
        roles(&fragments),
        vec![
            Some("user"),
            Some("assistant"),
            Some("user"),
            Some("assistant"),
            Some("system")
        ]
    );
}

#[test]
fn bracket_bold_and_dash_markers() {
    let text =
        "[user] q1\n[assistant]: a1\n**user**: q2\n**assistant:** a2\nUser — q3\nAssistant – a3";
    let fragments = segment(text, 400);
    assert_eq!(
        roles(&fragments),
        vec![
            Some("user"),
            Some("assistant"),
            Some("user"),
            Some("assistant"),
            Some("user"),
            Some("assistant")
        ]
    );
}

#[test]
fn unknown_prefixes_are_not_markers() {
    let text =
        "Note: this is not a role\nuser: real turn\nTODO: still inside the turn\nassistant: reply";
    let fragments = segment(text, 400);
    assert_eq!(
        roles(&fragments),
        vec![None, Some("user"), Some("assistant")]
    );
    assert_eq!(fragments[0].text, "Note: this is not a role\n");
    assert_eq!(
        fragments[1].text,
        "user: real turn\nTODO: still inside the turn\n"
    );
}

/// A single marker-looking line inside prose is not a transcript: no split,
/// no invented author. Two markers without a user side are not one either.
#[test]
fn lone_or_userless_markers_do_not_split_prose() {
    let prose = "Quarterly notes.\nAI: adoption grew 40% this year.\nNext steps follow.";
    let fragments = segment(prose, 400);
    assert_eq!(roles(&fragments), vec![None]);
    assert_eq!(fragments[0].text, prose);

    let log = "System: boot ok\nSystem: disk ok\nAssistant: ready";
    let fragments = segment(log, 400);
    assert_eq!(roles(&fragments), vec![None]);
    assert_eq!(fragments[0].text, log);

    let one_turn = "user: just one line";
    let fragments = segment(one_turn, 400);
    assert_eq!(roles(&fragments), vec![None]);
}

/// The smallest real transcript — one user line, one reply — still parses.
#[test]
fn two_line_transcript_still_parses() {
    let fragments = segment("Human: where are my keys?\nAI: in the jacket.", 400);
    assert_eq!(roles(&fragments), vec![Some("user"), Some("assistant")]);
    assert_eq!(fragments[0].text, "Human: where are my keys?\n");
    assert_eq!(fragments[1].text, "AI: in the jacket.");
}

#[test]
fn whitespace_prefix_folds_into_first_turn() {
    let fragments = segment("\n\nuser: hi\nassistant: yo", 400);
    assert_eq!(roles(&fragments), vec![Some("user"), Some("assistant")]);
    assert_eq!(fragments[0].byte_start, 0);
    assert_eq!(fragments[0].text, "\n\nuser: hi\n");
}

#[test]
fn jsonl_turns_take_role_from_object() {
    let text = "{\"role\": \"User\", \"content\": \"hi\"}\n\n{\"role\":\"Assistant\",\"content\":\"yo\"}\n{\"role\":\"tool\",\"content\":\"x\"}\n";
    let fragments = segment(text, 400);
    assert_eq!(
        roles(&fragments),
        vec![Some("user"), Some("assistant"), Some("tool")]
    );
    assert_eq!(
        fragments[0].text,
        "{\"role\": \"User\", \"content\": \"hi\"}\n\n"
    );
    assert_eq!(fragments[2].text, "{\"role\":\"tool\",\"content\":\"x\"}\n");
}

#[test]
fn jsonl_requires_every_line_to_qualify() {
    let text = "{\"role\": \"user\", \"content\": \"hi\"}\nnot json\n";
    let fragments = segment(text, 400);
    assert_eq!(fragments.len(), 1);
    assert_eq!(fragments[0].role, None);
}

#[test]
fn long_turn_prefers_paragraph_then_sentence_boundaries() {
    let paragraph = "First sentence here. Second sentence here. Third sentence here.";
    let text = format!("user: {paragraph}\n\n{paragraph}\n\n{paragraph}\nassistant: ok");
    // Budget: ~1 paragraph (63 chars ≈ 21 tokens) + marker.
    let all = segment(&text, 30);
    assert_well_formed(&source(&text), &all, 30);
    assert_eq!(all.last().unwrap().text, "assistant: ok");
    let fragments = &all[..all.len() - 1];
    assert!(fragments.len() >= 3, "got {}", fragments.len());
    assert!(fragments.iter().all(|f| f.role.as_deref() == Some("user")));
    assert!(fragments.iter().all(|f| f.turn_index == 0));
    // Paragraph cuts: every fragment after the first starts a paragraph.
    for f in &fragments[1..] {
        assert!(f.text.starts_with("First sentence"), "{:?}", f.text);
    }

    // A tighter budget forces sentence-level cuts inside each paragraph.
    let all = segment(&text, 10);
    assert_well_formed(&source(&text), &all, 10);
    let tight = &all[..all.len() - 1];
    assert!(tight.len() > fragments.len());
    for f in &tight[1..] {
        assert!(
            f.text.starts_with("First")
                || f.text.starts_with("Second")
                || f.text.starts_with("Third"),
            "{:?}",
            f.text
        );
    }
}

#[test]
fn long_text_without_delimiters_hard_splits_on_char_boundaries() {
    let text = "ą".repeat(500);
    let fragments = segment(&text, 10);
    assert_well_formed(&source(&text), &fragments, 10);
    assert!(fragments.len() >= 17);
    assert!(fragments.iter().all(|f| f.text.chars().all(|c| c == 'ą')));
}

#[test]
fn multibyte_text_is_never_split_inside_a_char() {
    let text = "user: Zażółć gęślą jaźń 🙂🙃 żółw\nassistant: Łódź — żółć 🇵🇱 końcówka. Następne zdanie? Tak! Koniec";
    for max in [1, 2, 3, 5, 8, 13, 400] {
        let fragments = segment(text, max);
        assert_well_formed(&source(text), &fragments, max);
        let rebuilt: String = fragments.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(rebuilt, text);
    }
}

#[test]
fn single_char_over_budget_is_emitted_as_is() {
    struct Huge;
    impl TokenCounter for Huge {
        fn count(&self, text: &str) -> Result<usize, FragmentError> {
            Ok(text.chars().count() * 100)
        }
    }
    let src = source("ab");
    let fragments = segment_source(&src, &cfg(1), &Huge).expect("segment");
    verify_invariants(&src, &fragments).expect("invariants");
    assert_eq!(fragments.len(), 2);
    assert_eq!(fragments[0].text, "a");
    assert_eq!(fragments[1].text, "b");
}

#[test]
fn appending_a_turn_leaves_earlier_fragments_byte_identical() {
    let base = "user: Tell me about Kraków.\n\nassistant: Kraków is a city on the Vistula. It was the royal capital. Many museums.\nuser: And Gdańsk?\n";
    let extended = format!(
        "{base}assistant: Gdańsk is a port on the Baltic. Amber everywhere. Long history.\n"
    );
    for max in [4, 12, 400] {
        let before = segment(base, max);
        let after = segment(&extended, max);
        assert!(after.len() > before.len());
        assert_eq!(&after[..before.len()], &before[..]);
    }
}

#[test]
fn segmentation_is_deterministic() {
    let text = "user: a b c d e f g h i j k l m n o p\nassistant: q r s t u v w x y z";
    assert_eq!(segment(text, 3), segment(text, 3));
}

/// Coverage and budget invariants on pseudo-random inputs (LCG, fixed seeds).
#[test]
fn random_inputs_keep_invariants() {
    const ALPHABET: [&str; 16] = [
        "a",
        "b",
        " ",
        " ",
        "\n",
        "\n\n",
        ". ",
        "? ",
        "! ",
        "ż",
        "🙂",
        "user: ",
        "assistant: ",
        "[system] ",
        "**user**: ",
        "\r\n",
    ];
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    for case in 0..200u32 {
        let mut text = String::new();
        let len = 1 + (case % 97);
        for _ in 0..len {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let pick = usize::try_from((state >> 33) % 16).expect("fits");
            text.push_str(ALPHABET[pick]);
        }
        let max = 1 + usize::try_from(case % 7).expect("fits");
        let src = source(&text);
        let fragments = segment_source(&src, &cfg(max), &ApproxTokenCounter).expect("segment");
        assert_well_formed(&src, &fragments, max);
        let rebuilt: String = fragments.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(rebuilt, text);
    }
}

#[test]
fn tiktoken_counter_counts_and_splits() {
    let counter = TiktokenCounter::new().expect("cl100k");
    assert_eq!(counter.count("").unwrap(), 0);
    assert!(counter.count("hello world").unwrap() >= 2);
    let text = format!(
        "user: {}",
        "The quick brown fox jumps over the lazy dog. ".repeat(40)
    );
    let src = source(&text);
    let fragments = segment_source(&src, &cfg(50), &counter).expect("segment");
    verify_invariants(&src, &fragments).expect("invariants");
    assert!(fragments.len() > 1);
    assert!(fragments
        .iter()
        .all(|f| counter.count(&f.text).unwrap() <= 50));
}

#[test]
fn verify_invariants_rejects_gaps_and_foreign_text() {
    let src = source("user: a\nassistant: b");
    let mut fragments = segment_source(&src, &cfg(400), &ApproxTokenCounter).expect("segment");
    fragments[1].text = "assistant: c".to_string();
    assert!(verify_invariants(&src, &fragments).is_err());
    fragments.pop();
    assert!(verify_invariants(&src, &fragments).is_err());
}

#[test]
fn fragment_chunk_fills_fragment_fields_and_keeps_template() {
    let src = source("user: hello\nassistant: world");
    let fragments = segment_source(&src, &cfg(400), &ApproxTokenCounter).expect("segment");
    let template = Chunk {
        id: "template".to_string(),
        content: "template".to_string(),
        stream: "__user_default__".to_string(),
        level: 1,
        score: 0.5,
        timestamp: 42,
        consolidated: false,
        dormant: false,
        in_progress: false,
        prompt_version: None,
        source_ids: None,
        last_decay: None,
        metadata: None,
        importance: None,
        persistent: false,
        last_implicit_boost: None,
        access_count: 0,
        source: None,
        created_by: None,
        updated_at: None,
        valid_from: None,
        valid_until: None,
        is_latest: true,
        superseded_by: None,
        supersedes_id: None,
        root_memory_id: None,
        version: 1,
        memory_type: None,
        extraction_meta: None,
        deleted_at: None,
        trust_level: Some("a2".to_string()),
        ingester_user_id: None,
        alpha: 1.0,
        beta: 1.0,
        harmful_count: 0,
        n_ratings: 0,
        last_rated_at: None,
        provenance_role: crate::storage::ProvenanceRole::Evidence,
        fragment: None,
    };
    let chunk = fragment_chunk(&fragments[1], &template);
    assert_eq!(chunk.id, "src-1:f1");
    assert_eq!(chunk.content, "assistant: world");
    assert_eq!(chunk.level, 0);
    assert_eq!(chunk.source_ids, Some(vec!["src-1".to_string()]));
    assert_eq!(chunk.stream, template.stream);
    assert_eq!(chunk.timestamp, 42);
    assert_eq!(chunk.trust_level.as_deref(), Some("a2"));
    assert_eq!(
        chunk.provenance_role,
        crate::storage::ProvenanceRole::Evidence
    );
    let meta = chunk.fragment.expect("fragment meta");
    assert_eq!(meta.source_id, "src-1");
    assert_eq!((meta.byte_start, meta.byte_end), (12, 28));
    assert_eq!(meta.turn_index, 1);
    assert_eq!(meta.role.as_deref(), Some("assistant"));
    assert_eq!(meta.conversation_date.as_deref(), Some("2026-10-21"));
}

#[test]
fn chunk_without_fragment_key_deserializes_as_none() {
    // A pre-fragments row has no `fragment` key at all.
    let json = serde_json::json!({
        "id": "old", "content": "c", "stream": "s", "level": 0, "score": 0.0,
        "timestamp": 1, "consolidated": false, "dormant": false, "in_progress": false,
        "prompt_version": null, "source_ids": null, "last_decay": null, "metadata": null
    });
    let chunk: Chunk = serde_json::from_value(json).expect("legacy chunk");
    assert!(chunk.fragment.is_none());
}

#[test]
fn config_defaults_are_off_with_documented_budgets() {
    let dflt = FragmentsConfig::default();
    assert!(!dflt.enabled);
    assert_eq!(dflt.max_fragment_tokens, 400);
    assert_eq!(dflt.prefix_reserve_tokens, 16);
    assert_eq!(dflt.window_tokens(), 416);
    let empty: FragmentsConfig = toml::from_str("").expect("empty section");
    assert_eq!(empty, dflt);
}

/// Shipped config.toml carries a `[fragments]` section that agrees with the
/// code Default; a config without the section loads and stays off.
#[test]
fn config_toml_fragments_section_matches_default_and_is_optional() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../config.toml");
    let content = std::fs::read_to_string(path).expect("read config.toml");
    let cfg: Config = toml::from_str(&content).expect("parse config.toml");
    assert_eq!(cfg.fragments, FragmentsConfig::default());
    assert!(content.contains("[fragments]"));

    let without: String = content
        .lines()
        .scan(false, |in_section, line| {
            if line.trim() == "[fragments]" {
                *in_section = true;
            } else if *in_section && line.trim_start().starts_with('[') {
                *in_section = false;
            }
            Some(if *in_section { None } else { Some(line) })
        })
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");
    let cfg: Config = toml::from_str(&without).expect("parse without [fragments]");
    assert_eq!(cfg.fragments, FragmentsConfig::default());
}

#[test]
fn config_role_and_counter_keys_default_and_reject_unknown_values() {
    let bare: FragmentsConfig = toml::from_str("enabled = true").expect("bare section");
    assert!(bare.enabled);
    assert_eq!(bare.provenance_role, ProvenanceRole::Claim);
    assert_eq!(bare.token_counter, TokenCounterKind::Approx);
    let set: FragmentsConfig =
        toml::from_str("provenance_role = \"evidence\"\ntoken_counter = \"tiktoken\"")
            .expect("explicit values");
    assert_eq!(set.provenance_role, ProvenanceRole::Evidence);
    assert_eq!(set.token_counter, TokenCounterKind::Tiktoken);
    let cue: FragmentsConfig = toml::from_str("provenance_role = \"cue\"").expect("cue");
    assert_eq!(cue.provenance_role, ProvenanceRole::Cue);
    let bad_role = toml::from_str::<FragmentsConfig>("provenance_role = \"Claim\"")
        .expect_err("capitalised role is rejected");
    assert!(bad_role.to_string().contains("provenance_role"));
    assert!(toml::from_str::<FragmentsConfig>("token_counter = \"bpe\"").is_err());
    let round_trip: FragmentsConfig =
        toml::from_str(&toml::to_string(&set).expect("serialize")).expect("round trip");
    assert_eq!(round_trip, set);
    assert_eq!(
        TokenCounterKind::Approx
            .counter()
            .unwrap()
            .count("abcdef")
            .unwrap(),
        2
    );
    assert!(
        TokenCounterKind::Tiktoken
            .counter()
            .unwrap()
            .count("hello world")
            .unwrap()
            >= 2
    );
    let embedder: FragmentsConfig =
        toml::from_str("token_counter = \"embedder\"").expect("embedder kind");
    assert_eq!(embedder.token_counter, TokenCounterKind::Embedder);
    let err = TokenCounterKind::Embedder
        .counter()
        .err()
        .expect("embedder counter is caller-supplied, never built here");
    assert!(matches!(err, FragmentError::Tokenizer(_)));
}

#[test]
fn a_failing_counter_fails_segmentation_instead_of_guessing() {
    struct Broken;
    impl TokenCounter for Broken {
        fn count(&self, _text: &str) -> Result<usize, FragmentError> {
            Err(FragmentError::Tokenizer("synthetic".into()))
        }
    }
    let err = segment_source(&source("user: hi"), &cfg(400), &Broken).expect_err("fails");
    assert!(matches!(err, FragmentError::Tokenizer(_)));
}

#[test]
fn validate_rejects_zero_budget_and_local_window_overflow() {
    let dflt = FragmentsConfig::default();
    dflt.validate(true).expect("defaults fit the local window");
    dflt.validate(false)
        .expect("defaults are valid for any provider");
    let zero = FragmentsConfig {
        max_fragment_tokens: 0,
        ..FragmentsConfig::default()
    };
    assert!(zero
        .validate(false)
        .unwrap_err()
        .to_string()
        .contains("max_fragment_tokens"));
    let wide = FragmentsConfig {
        max_fragment_tokens: 500,
        prefix_reserve_tokens: 16,
        ..FragmentsConfig::default()
    };
    assert_eq!(wide.window_tokens(), 516);
    assert!(wide
        .validate(true)
        .unwrap_err()
        .to_string()
        .contains("512-token window"));
    wide.validate(false)
        .expect("a remote provider's window is not checked");
    let exact = FragmentsConfig {
        max_fragment_tokens: LOCAL_EMBEDDER_WINDOW_TOKENS - 16,
        prefix_reserve_tokens: 16,
        ..FragmentsConfig::default()
    };
    exact.validate(true).expect("exactly the window is allowed");
}

#[test]
fn validate_rejects_embedder_counter_without_the_local_provider() {
    let embedder = FragmentsConfig {
        enabled: true,
        token_counter: TokenCounterKind::Embedder,
        ..FragmentsConfig::default()
    };
    embedder
        .validate(true)
        .expect("the local provider loads the embedder tokenizer");
    let err = embedder.validate(false).unwrap_err().to_string();
    assert!(
        err.contains("token_counter = \"embedder\" requires embedding_provider = \"local\""),
        "{err}"
    );
    // With fragments off the counter is never used, so the combination is allowed.
    let disabled = FragmentsConfig {
        enabled: false,
        ..embedder
    };
    disabled
        .validate(false)
        .expect("a disabled section does not need the tokenizer");
}
