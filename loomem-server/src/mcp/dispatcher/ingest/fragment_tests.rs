//! Opt-in source fragments on the MCP ingest path, over real storage.
use super::fixtures::*;

use loomem_core::fragments::fragment_id;
use loomem_core::storage::{is_ingest_fragment, Chunk, ProvenanceRole};
use std::sync::Arc;

use super::*;

const STREAM: &str = "fragments";
const TRANSCRIPT: &str = "user: I prefer quiet accommodation.\nassistant: A lively hotel is another option.\nuser: Please keep my original preference.";

fn enabled(role: ProvenanceRole) -> (axum::Router, Arc<AppState>) {
    let (_, original) = fixture(20000);
    let mut config = original.config.clone();
    config.fragments.enabled = true;
    config.fragments.provenance_role = role;
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
