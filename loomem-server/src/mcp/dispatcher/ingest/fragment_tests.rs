//! Opt-in source fragments on the MCP ingest path, over real storage.
use super::fixtures::*;

use loomem_core::fragments::{
    fragment_id, verify_invariants, CanonicalSource, SourceFragment, TokenCounterKind,
};
use loomem_core::storage::{is_ingest_fragment, Chunk, ProvenanceRole};
use std::sync::Arc;

use super::*;

const STREAM: &str = "fragments";
const TRANSCRIPT: &str = "user: I prefer quiet accommodation.\nassistant: A lively hotel is another option.\nuser: Please keep my original preference.";

/// The test app loads no local embedding model, so the default `"embedder"`
/// counter is unavailable; these tests pin the model-free one explicitly.
fn enabled(role: ProvenanceRole) -> (axum::Router, Arc<AppState>) {
    let (_, original) = fixture(20000);
    let mut config = original.config.clone();
    config.fragments.enabled = true;
    config.fragments.provenance_role = role;
    config.fragments.token_counter = TokenCounterKind::Approx;
    crate::tests::make_test_app_cfg(config)
}

fn by_kind<'a>(records: &'a [Chunk], kind_name: &str) -> Vec<&'a Chunk> {
    records
        .iter()
        .filter(|chunk| kind(chunk) == kind_name)
        .collect()
}

fn persisted(state: &AppState, id: &str) -> Chunk {
    state.store.get_chunk(id).unwrap().unwrap()
}

#[tokio::test]
async fn disabled_by_default_keeps_chunk_set_and_tool_text_identical() {
    let (_, state) = fixture(20000);
    assert!(!state.config.fragments.enabled);
    let chat = Chat::facts(vec![fact(None)]);
    let result = tool_ingest_with(&chat, &state, args(TRANSCRIPT), STREAM, None)
        .await
        .unwrap();
    let records = chunks(&state, STREAM);
    assert_eq!(records.len(), 2);
    assert!(by_kind(&records, "fragment").is_empty());
    let source = by_kind(&records, "source")[0];
    assert_eq!(
        result.content[0].text,
        format!(
            "Source stored (id: {}). Extracted 1 facts from conversation, stored 1, skipped 0.",
            source.id
        )
    );
}

#[tokio::test]
async fn enabled_persists_source_fragments_and_facts_with_exact_slices() {
    let (_, state) = enabled(ProvenanceRole::Claim);
    let chat = Chat::facts(vec![fact(None)]);
    let result = tool_ingest_with(&chat, &state, args(TRANSCRIPT), STREAM, None)
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    let records = chunks(&state, STREAM);
    let source = by_kind(&records, "source")[0];
    let derived = by_kind(&records, "fact");
    let mut fragments = by_kind(&records, "fragment");
    assert_eq!(derived.len(), 1);
    assert_eq!(fragments.len(), 3);
    assert_eq!(records.len(), 5);
    assert_link(derived[0], source);
    fragments.sort_by_key(|chunk| chunk.fragment.as_ref().unwrap().byte_start);
    let mut cursor = 0;
    for (index, fragment) in fragments.iter().enumerate() {
        let index = u32::try_from(index).unwrap();
        let meta = fragment.fragment.as_ref().unwrap();
        assert_eq!(fragment.id, fragment_id(&source.id, index));
        assert_eq!(meta.source_id, source.id);
        assert_eq!(meta.byte_start, cursor);
        assert_eq!(
            fragment.content,
            &source.content[meta.byte_start..meta.byte_end]
        );
        assert_eq!(meta.turn_index, index);
        assert_eq!(meta.conversation_date.as_deref(), Some("2024-03-20"));
        assert_eq!(fragment.level, 0);
        assert_eq!(
            fragment.source_ids.as_deref(),
            Some([source.id.clone()].as_slice())
        );
        assert!(fragment.extraction_meta.is_none());
        assert_eq!(fragment.provenance_role, ProvenanceRole::Claim);
        assert_eq!(fragment.trust_level, source.trust_level);
        assert_eq!(fragment.source.as_ref().unwrap().agent, "mcp-ingest");
        assert_eq!(
            fragment.metadata.as_ref().unwrap()["_loomem_ingest"]["conversation_date"],
            "2024-03-20"
        );
        assert!(is_ingest_fragment(fragment));
        cursor = meta.byte_end;
    }
    assert_eq!(cursor, source.content.len());
    let roles: Vec<_> = fragments
        .iter()
        .map(|chunk| chunk.fragment.as_ref().unwrap().role.as_deref())
        .collect();
    assert_eq!(roles, [Some("user"), Some("assistant"), Some("user")]);
    assert!(result.content[0].text.starts_with(&format!(
        "Source stored (id: {}). Extracted 1 facts from conversation, stored 1, skipped 0.",
        source.id
    )));
    assert!(result.content[0].text.ends_with(" Stored 3 fragments."));
    let indexed = state
        .tantivy
        .lock()
        .await
        .search_with_stream("lively hotel", STREAM, 10)
        .unwrap();
    assert!(indexed.iter().any(|hit| hit.id == fragments[1].id));
}

#[tokio::test]
async fn fragment_role_follows_config_and_extraction_request_is_unchanged() {
    let (_, baseline_state) = fixture(20000);
    let baseline_chat = Chat::facts(vec![fact(None)]);
    tool_ingest_with(
        &baseline_chat,
        &baseline_state,
        args(TRANSCRIPT),
        STREAM,
        None,
    )
    .await
    .unwrap();
    let (_, state) = enabled(ProvenanceRole::Evidence);
    let chat = Chat::facts(vec![fact(None)]);
    tool_ingest_with(&chat, &state, args(TRANSCRIPT), STREAM, None)
        .await
        .unwrap();
    assert_eq!(
        *baseline_chat.requests.lock().unwrap(),
        *chat.requests.lock().unwrap()
    );
    let records = chunks(&state, STREAM);
    let fragments = by_kind(&records, "fragment");
    assert_eq!(fragments.len(), 3);
    assert!(fragments
        .iter()
        .all(|chunk| chunk.provenance_role == ProvenanceRole::Evidence));
    assert_eq!(
        by_kind(&records, "source")[0].provenance_role,
        ProvenanceRole::Evidence
    );
    assert_eq!(
        by_kind(&records, "fact")[0].provenance_role,
        ProvenanceRole::Claim
    );
}

#[tokio::test]
async fn fragments_are_slices_of_the_sanitized_persisted_source() {
    let (_, state) = enabled(ProvenanceRole::Claim);
    let address = "synthetic-person@example.invalid";
    let transcript =
        format!("<b>user:</b> My contact is {address}.\nassistant: <i>Noted</i>, {address}.");
    let chat = Chat::facts(vec![fact(None)]);
    tool_ingest_with(&chat, &state, args(&transcript), STREAM, None)
        .await
        .unwrap();
    let records = chunks(&state, STREAM);
    let source = persisted(&state, &by_kind(&records, "source")[0].id);
    let fragments = by_kind(&records, "fragment");
    assert!(!fragments.is_empty());
    for fragment in fragments {
        let stored = persisted(&state, &fragment.id);
        let meta = stored.fragment.as_ref().unwrap();
        assert_eq!(
            stored.content,
            &source.content[meta.byte_start..meta.byte_end]
        );
        let serialized = serde_json::to_string(&stored).unwrap();
        assert!(!serialized.contains(address));
        assert!(!serialized.contains("<b>") && !serialized.contains("<i>"));
    }
}

/// Rebuild the fragment set of `source` from what the store holds, so the
/// domain invariants can be re-checked over the persisted bytes.
fn persisted_fragments(state: &AppState, source: &Chunk) -> (CanonicalSource, Vec<SourceFragment>) {
    let mut fragments: Vec<SourceFragment> = chunks(state, STREAM)
        .into_iter()
        .filter(|chunk| kind(chunk) == "fragment")
        .map(|chunk| persisted(state, &chunk.id))
        .map(|chunk| {
            let meta = chunk.fragment.clone().unwrap();
            let index: u32 = chunk.id.rsplit(":f").next().unwrap().parse().unwrap();
            SourceFragment {
                id: chunk.id.clone(),
                source_id: meta.source_id,
                index,
                byte_start: meta.byte_start,
                byte_end: meta.byte_end,
                turn_index: meta.turn_index,
                role: meta.role,
                conversation_date: meta.conversation_date,
                text: chunk.content,
            }
        })
        .collect();
    fragments.sort_by_key(|fragment| fragment.byte_start);
    let canonical = CanonicalSource {
        id: source.id.clone(),
        text: persisted(state, &source.id).content,
        conversation_date: Some("2024-03-20".into()),
    };
    (canonical, fragments)
}

/// BLOCKER-1 repro: HTML entities decode on one sanitiser pass and are
/// stripped as tags on the next, so the persisted source is shorter than a
/// single pass of the input. Ranges must index into the persisted bytes.
#[tokio::test]
async fn fragments_index_into_the_persisted_source_after_entity_decoding() {
    for (name, transcript) in [
        ("single", "user: say &lt;b&gt;hi&lt;/b&gt;\nassistant: ok"),
        (
            "double",
            "user: say &amp;lt;b&amp;gt;hi&amp;lt;/b&amp;gt;\nassistant: ok",
        ),
    ] {
        let (_, state) = enabled(ProvenanceRole::Claim);
        let chat = Chat::facts(vec![fact(None)]);
        let result = tool_ingest_with(&chat, &state, args(transcript), STREAM, None)
            .await
            .unwrap();
        assert_ne!(result.is_error, Some(true), "{name}");
        let records = chunks(&state, STREAM);
        let source = persisted(&state, &by_kind(&records, "source")[0].id);
        assert_eq!(source.content, "user: say hi\nassistant: ok", "{name}");
        let (canonical, fragments) = persisted_fragments(&state, &source);
        assert_eq!(fragments.len(), 2, "{name}");
        for fragment in &fragments {
            assert_eq!(
                fragment.text,
                &canonical.text[fragment.byte_start..fragment.byte_end],
                "{name}: {}",
                fragment.id
            );
        }
        verify_invariants(&canonical, &fragments).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(
            result.content[0].text.ends_with(" Stored 2 fragments."),
            "{name}"
        );
    }
}

/// Input that is still drifting after the pass budget is refused up front,
/// with nothing stored.
#[tokio::test]
async fn deeply_encoded_content_is_refused_before_any_write() {
    let (_, state) = enabled(ProvenanceRole::Claim);
    let chat = Chat::facts(vec![fact(None)]);
    let quad = "user: &amp;amp;amp;lt;b&amp;amp;amp;gt;x\nassistant: ok";
    let error = tool_ingest_with(&chat, &state, args(quad), STREAM, None)
        .await
        .expect_err("refused");
    assert!(
        error.message.contains("did not stabilise"),
        "{}",
        error.message
    );
    assert!(chunks(&state, STREAM).is_empty());
    assert_eq!(chat.calls(), 0);
}

/// A turn the sink would only trim (an `assistant:` line holding a lone `<`
/// ends with the newline that belongs to it) is stored with its range
/// narrowed to the surviving bytes, still an exact slice of the source.
#[tokio::test]
async fn trimmed_fragment_ranges_follow_the_stored_bytes() {
    let (_, state) = enabled(ProvenanceRole::Claim);
    let chat = Chat::facts(vec![fact(None)]);
    let transcript = "user: is 1 < 2?\nassistant: yes, 1 < 2\nuser: thanks";
    let result = tool_ingest_with(&chat, &state, args(transcript), STREAM, None)
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    let records = chunks(&state, STREAM);
    let source = persisted(&state, &by_kind(&records, "source")[0].id);
    assert_eq!(source.content, transcript);
    let (canonical, fragments) = persisted_fragments(&state, &source);
    assert_eq!(fragments.len(), 3);
    for fragment in &fragments {
        assert_eq!(
            fragment.text,
            &canonical.text[fragment.byte_start..fragment.byte_end],
            "{}",
            fragment.id
        );
    }
    assert_eq!(fragments[1].text, "assistant: yes, 1 < 2");
    assert_eq!(fragments[1].byte_end, fragments[2].byte_start - 1);
    assert!(result.content[0].text.ends_with(" Stored 3 fragments."));
}

#[test]
fn align_to_sink_narrows_trimmed_slices_and_rejects_rewritten_ones() {
    let (_, state) = enabled(ProvenanceRole::Claim);
    let fragment = SourceFragment {
        id: "src:f1".into(),
        source_id: "src".into(),
        index: 1,
        byte_start: 10,
        byte_end: 32,
        turn_index: 1,
        role: Some("assistant".into()),
        conversation_date: None,
        text: "assistant: yes, 1 < 2\n".into(),
    };
    let aligned = fragments::align_to_sink(&state.pii_filter, &fragment).unwrap();
    assert_eq!(aligned.text, "assistant: yes, 1 < 2");
    assert_eq!((aligned.byte_start, aligned.byte_end), (10, 31));
    let plain = SourceFragment {
        text: "user: plain\n".into(),
        ..fragment.clone()
    };
    assert_eq!(
        fragments::align_to_sink(&state.pii_filter, &plain).unwrap(),
        plain
    );
    let rewritten = SourceFragment {
        text: "assistant: mail synthetic-person@example.invalid\n".into(),
        ..fragment.clone()
    };
    assert!(fragments::align_to_sink(&state.pii_filter, &rewritten).is_none());
}

/// BLOCKER-2: fragments are consolidated from birth, so they never enter the
/// L0 consolidation scan, the pending-L0 counter or "recent" context; their
/// search line carries the conversation date like the source's does.
#[tokio::test]
async fn fragments_stay_out_of_consolidation_recent_context_and_carry_observed_date() {
    let (_, state) = enabled(ProvenanceRole::Claim);
    let chat = Chat::facts(vec![fact(None)]);
    tool_ingest_with(&chat, &state, args(TRANSCRIPT), STREAM, None)
        .await
        .unwrap();
    let records = chunks(&state, STREAM);
    let fragments = by_kind(&records, "fragment");
    assert_eq!(fragments.len(), 3);
    assert!(fragments.iter().all(|chunk| chunk.consolidated));
    let pending = state.store.scan_l0_unconsolidated(0, 100).unwrap();
    assert!(pending
        .iter()
        .all(|chunk| chunk.stream != STREAM || chunk.fragment.is_none()));

    let context = crate::mcp::dispatcher::tool_context(
        &state,
        serde_json::json!({"query":"accommodation", "budget_tokens":8000, "sections":["recent"]}),
        STREAM,
    )
    .await
    .unwrap();
    let text = &context.content[0].text;
    let (_, recent) = text.split_once("## Recent Context\n\n").unwrap();
    let lines: Vec<&str> = recent
        .lines()
        .filter(|line| line.starts_with("- ["))
        .collect();
    assert_eq!(lines.len(), 2, "source + fact only: {recent}");
    assert!(lines
        .iter()
        .any(|line| line.starts_with("- [observed:2024-03-20] user: I prefer")));

    let search = crate::mcp::dispatcher::tool_search(
        &state,
        serde_json::json!({"query":"lively hotel", "top_k":20}),
        STREAM,
    )
    .await
    .unwrap();
    assert_ne!(search.is_error, Some(true));
    let fragment = fragments
        .iter()
        .find(|chunk| chunk.content.contains("lively hotel"))
        .unwrap();
    // The fragment's text ends with its newline, so its entry spans two
    // physical lines: locate the entry by its id and read back to "N. [".
    let text = &search.content[0].text;
    let id_at = text
        .find(&format!("(id: {})", fragment.id))
        .unwrap_or_else(|| panic!("fragment entry missing: {text}"));
    let entry_at = text[..id_at].rfind(". [").unwrap() + 2;
    let entry = &text[entry_at..id_at];
    assert!(entry.starts_with("[observed:2024-03-20]"), "{entry}");
    assert!(!entry.contains("[2026-"), "{entry}");
}

/// A sink whose stored source does not match the segmented text (synthetic
/// drift) yields no fragments at all — never ranges into the wrong bytes.
struct DriftingReadBack;

impl IngestPersistence for DriftingReadBack {
    async fn persist(
        &self,
        state: &Arc<crate::AppState>,
        chunk: Chunk,
    ) -> Result<String, crate::handlers::AppError> {
        RealPersistence.persist(state, chunk).await
    }

    async fn persisted(
        &self,
        state: &Arc<crate::AppState>,
        id: &str,
    ) -> Result<Option<Chunk>, crate::handlers::AppError> {
        let mut stored = RealPersistence.persisted(state, id).await?;
        if let Some(chunk) = stored.as_mut() {
            chunk.content.push_str(" (drifted)");
        }
        Ok(stored)
    }
}

#[tokio::test]
async fn a_source_that_reads_back_differently_gets_no_fragments() {
    let (_, state) = enabled(ProvenanceRole::Claim);
    let chat = Chat::facts(vec![fact(None)]);
    let result = tool_ingest_with_sink(
        &chat,
        &DriftingReadBack,
        &state,
        args(TRANSCRIPT),
        STREAM,
        None,
    )
    .await
    .unwrap();
    assert_ne!(result.is_error, Some(true));
    let records = chunks(&state, STREAM);
    assert!(by_kind(&records, "fragment").is_empty());
    assert_eq!(by_kind(&records, "fact").len(), 1);
    assert!(result.content[0]
        .text
        .ends_with(" Stored 0 fragments (segmentation skipped: persisted source differs)."));
}

/// `"embedder"` (the default counter) never falls back to an approximation:
/// without a loaded local model the tool says so and stores no fragment.
#[tokio::test]
async fn embedder_counter_without_a_loaded_model_fails_segmentation_loudly() {
    let (_, original) = fixture(20000);
    let mut config = original.config.clone();
    config.fragments.enabled = true;
    assert_eq!(config.fragments.token_counter, TokenCounterKind::Embedder);
    let (_, state) = crate::tests::make_test_app_cfg(config);
    assert!(state.local_embedder.is_none());
    let chat = Chat::facts(vec![fact(None)]);
    let result = tool_ingest_with(&chat, &state, args(TRANSCRIPT), STREAM, None)
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    let records = chunks(&state, STREAM);
    assert!(by_kind(&records, "fragment").is_empty());
    assert_eq!(by_kind(&records, "fact").len(), 1);
    assert!(result.content[0].text.ends_with(
        " Stored 0 fragments (segmentation failed: embedder tokenizer unavailable (no local embedding model loaded))."
    ), "{}", result.content[0].text);
}

struct FailWrite {
    fail_at: usize,
    writes: std::sync::atomic::AtomicUsize,
}

impl IngestPersistence for FailWrite {
    async fn persist(
        &self,
        state: &Arc<crate::AppState>,
        chunk: Chunk,
    ) -> Result<String, crate::handlers::AppError> {
        let step = self
            .writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        if step == self.fail_at {
            return Err(crate::handlers::AppError::Internal(anyhow::anyhow!(
                "synthetic write failure"
            )));
        }
        RealPersistence.persist(state, chunk).await
    }
}

#[tokio::test]
async fn fragment_write_failure_is_reported_and_facts_are_still_extracted() {
    let (_, state) = enabled(ProvenanceRole::Claim);
    let chat = Chat::facts(vec![fact(None)]);
    let sink = FailWrite {
        fail_at: 3,
        writes: std::sync::atomic::AtomicUsize::new(0),
    };
    let result = tool_ingest_with_sink(&chat, &sink, &state, args(TRANSCRIPT), STREAM, None)
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    assert_eq!(chat.calls(), 1);
    let records = chunks(&state, STREAM);
    assert_eq!(by_kind(&records, "source").len(), 1);
    assert_eq!(by_kind(&records, "fragment").len(), 2);
    assert_eq!(by_kind(&records, "fact").len(), 1);
    assert!(result.content[0].text.contains("stored 1, skipped 0."));
    assert!(result.content[0]
        .text
        .ends_with(" Stored 2 fragments, 1 fragment writes failed."));
}

#[tokio::test]
async fn extraction_disabled_still_reports_fragments() {
    let (_, original) = crate::tests::make_test_app();
    let mut config = original.config.clone();
    config.fragments.enabled = true;
    config.fragments.token_counter = TokenCounterKind::Approx;
    let (_, state) = crate::tests::make_test_app_cfg(config);
    let result = tool_ingest(&state, args(TRANSCRIPT), STREAM, None)
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    let records = chunks(&state, STREAM);
    assert_eq!(by_kind(&records, "fragment").len(), 3);
    assert!(result.content[0]
        .text
        .contains("Knowledge extraction disabled. Stored 3 fragments."));
}

#[tokio::test]
async fn deleting_the_source_tombstones_every_fragment_everywhere() {
    let (app, state) = enabled(ProvenanceRole::Claim);
    let chat = Chat::facts(vec![fact(None)]);
    tool_ingest_with(&chat, &state, args(TRANSCRIPT), STREAM, None)
        .await
        .unwrap();
    let records = chunks(&state, STREAM);
    let source = by_kind(&records, "source")[0];
    let fragment_ids: Vec<String> = by_kind(&records, "fragment")
        .iter()
        .map(|chunk| chunk.id.clone())
        .collect();
    assert_eq!(fragment_ids.len(), 3);
    let outcome = crate::handlers::delete::delete_memory_fully(
        &state.store,
        &state.tantivy,
        &state.graph,
        &source.id,
    )
    .await
    .unwrap();
    assert!(outcome.all_ok());
    assert!(chunks(&state, STREAM).is_empty());
    for id in &fragment_ids {
        assert!(persisted(&state, id).deleted_at.is_some());
        assert_eq!(get(&app, id).await.0, axum::http::StatusCode::NOT_FOUND);
    }
    let stale = state
        .tantivy
        .lock()
        .await
        .search_with_stream("lively hotel", STREAM, 10)
        .unwrap();
    assert!(!stale.iter().any(|hit| fragment_ids.contains(&hit.id)));
    let repaired = state.store.ingest_chunks_for_index_repair().unwrap();
    assert!(fragment_ids
        .iter()
        .all(|id| repaired.iter().any(|chunk| &chunk.id == id)));
}
