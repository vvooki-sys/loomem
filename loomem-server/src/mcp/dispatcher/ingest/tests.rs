//! Phase15 source/date contract exercised with injected chat and real storage.
#[path = "fixtures.rs"]
mod fixtures;
use fixtures::*;

use loomem_core::memory_extractor::ChatHttpReply;
use loomem_core::storage::ProvenanceRole;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Notify;

use super::*;

#[tokio::test]
async fn source_and_fact_persist_separate_provenance_without_fabricated_date() {
    let (app, state) = fixture(20000);
    let chat = Chat::facts(vec![fact(None)]);
    let transcript = "user: I prefer quiet accommodation.\nassistant: A lively hotel is another option.\nuser: Please keep my original preference.";
    let result = tool_ingest_with(
        &chat,
        &state,
        args(transcript),
        "phase15",
        Some("synthetic-user".into()),
    )
    .await
    .unwrap();
    assert_ne!(result.is_error, Some(true));
    let records = chunks(&state, "phase15");
    assert_eq!(records.len(), 2);
    let source = records
        .iter()
        .find(|chunk| kind(chunk) == "source")
        .unwrap();
    let derived = records.iter().find(|chunk| kind(chunk) == "fact").unwrap();
    assert_eq!(source.content, transcript);
    assert_eq!(source.level, 0);
    assert!(source.persistent && source.consolidated && !source.dormant);
    assert_eq!(source.provenance_role, ProvenanceRole::Evidence);
    assert_eq!(source.trust_level.as_deref(), Some("a2"));
    assert_eq!(source.source.as_ref().unwrap().agent, "mcp-ingest-source");
    assert_eq!(source.valid_from, None);
    assert_link(derived, source);
    assert_eq!(derived.provenance_role, ProvenanceRole::Claim);
    assert_eq!(derived.valid_from, None);
    assert_eq!(derived.extraction_meta.as_ref().unwrap().event_date, None);
    assert_eq!(
        source.metadata.as_ref().unwrap()["_loomem_ingest"]["conversation_date"],
        "2024-03-20"
    );
    assert!(result.content[0].text.contains(&source.id));
    let (status, body) = get(&app, &source.id).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["content"], transcript);
    let indexed = state
        .tantivy
        .lock()
        .await
        .search_with_stream("original preference", "phase15", 10)
        .unwrap();
    assert!(indexed.iter().any(|hit| hit.id == source.id));
    assert_eq!(state.store.get_all_chunks().unwrap().len(), records.len());
}

#[tokio::test]
async fn explicit_fact_date_is_distinct_from_conversation_date() {
    let (_, state) = fixture(20000);
    let chat = Chat::facts(vec![fact(Some("2024-03-19"))]);
    tool_ingest_with(
        &chat,
        &state,
        args("user: The workshop was yesterday."),
        "phase15",
        None,
    )
    .await
    .unwrap();
    let records = chunks(&state, "phase15");
    let derived = records.iter().find(|chunk| kind(chunk) == "fact").unwrap();
    let meta = derived.extraction_meta.as_ref().unwrap();
    assert_eq!(meta.event_date.as_deref(), Some("2024-03-19"));
    assert_eq!(derived.valid_from, meta.event_date_unix());
    assert_eq!(
        derived.metadata.as_ref().unwrap()["_loomem_ingest"]["conversation_date"],
        "2024-03-20"
    );
}

#[tokio::test]
async fn malformed_conversation_date_rejects_before_source_or_chat() {
    for date in ["not-a-date", "2024-02-30", "2024-3-2"] {
        let (_, state) = fixture(20000);
        let chat = Chat::facts(vec![fact(None)]);
        let input = json!({"content":"synthetic source","conversation_date":date});
        let result = tool_ingest_with(&chat, &state, input, "phase15", None).await;
        assert!(result.is_err() || result.unwrap().is_error == Some(true));
        assert_eq!(chat.calls(), 0);
        assert!(chunks(&state, "phase15").is_empty());
    }
}

#[tokio::test]
async fn source_and_every_derived_field_are_sanitized_before_persistence() {
    let (_, state) = fixture(20000);
    let address = "synthetic-person@example.invalid";
    let transcript = format!("<b>user:</b> My contact is {address}. I prefer quiet accommodation.");
    let canonical = state
        .pii_filter
        .sanitize(&loomem_core::sanitizer::sanitize(&transcript).content)
        .0;
    let mut injected = fact(None);
    for field in ["content", "subject", "event_date_context", "attributed_to"] {
        injected[field] = json!(format!("<b>synthetic</b> {address}"));
    }
    let chat = Chat::facts(vec![injected]);
    tool_ingest_with(&chat, &state, args(&transcript), "phase15", None)
        .await
        .unwrap();
    let records = chunks(&state, "phase15");
    assert_eq!(records.len(), 2);
    let source = records
        .iter()
        .find(|chunk| kind(chunk) == "source")
        .unwrap();
    assert_eq!(source.content, canonical);
    for record in &records {
        let serialized = serde_json::to_string(record).unwrap();
        assert!(!serialized.contains(address));
        assert!(!serialized.contains("<b>"));
    }
    let requests = chat.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].to_string().contains(address));
    assert!(!requests[0].to_string().contains("<b>"));
}

#[tokio::test]
async fn zero_facts_and_failed_extraction_retain_retrievable_source() {
    for reply in [
        Ok(reply(vec![])),
        Ok(ChatHttpReply {
            status: 429,
            body: "synthetic quota failure".into(),
        }),
        Err(anyhow::anyhow!("synthetic transport failure")),
    ] {
        let (app, state) = fixture(20000);
        let chat = Chat::sequence(vec![reply]);
        let result = tool_ingest_with(
            &chat,
            &state,
            args("user: Synthetic source retained."),
            "phase15",
            None,
        )
        .await
        .unwrap();
        let records = chunks(&state, "phase15");
        assert_eq!(records.len(), 1);
        assert_eq!(kind(&records[0]), "source");
        assert!(result.content[0].text.contains(&records[0].id));
        assert_eq!(
            get(&app, &records[0].id).await.0,
            axum::http::StatusCode::OK
        );
    }
}

#[tokio::test]
async fn missing_key_retains_source_and_reports_source_id() {
    let (_, state) = fixture(20000);
    let result = tool_ingest(
        &state,
        args("user: Synthetic source without extraction key."),
        "phase15",
        None,
    )
    .await
    .unwrap();
    assert_eq!(result.is_error, Some(true));
    let records = chunks(&state, "phase15");
    assert_eq!(records.len(), 1);
    assert_eq!(kind(&records[0]), "source");
    assert!(result.content[0].text.contains(&records[0].id));
}

#[tokio::test]
async fn cancellation_after_chat_entry_leaves_committed_source_only() {
    let (_, state) = fixture(20000);
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let chat = BlockedChat {
        entered: entered.clone(),
        release,
    };
    let task_state = state.clone();
    let task = tokio::spawn(async move {
        tool_ingest_with(
            &chat,
            &task_state,
            args("user: Synthetic cancellation source."),
            "phase15",
            None,
        )
        .await
    });
    entered.notified().await;
    let before = chunks(&state, "phase15");
    assert_eq!(before.len(), 1);
    assert_eq!(kind(&before[0]), "source");
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let after = chunks(&state, "phase15");
    assert_eq!(after.len(), 1);
    assert_eq!(before[0].id, after[0].id);
}

#[tokio::test]
async fn deleting_source_while_chat_is_blocked_rejects_late_facts() {
    let (app, state) = fixture(20000);
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let chat = BlockedChat {
        entered: entered.clone(),
        release: release.clone(),
    };
    let task_state = state.clone();
    let task = tokio::spawn(async move {
        tool_ingest_with(
            &chat,
            &task_state,
            args("user: Synthetic source deleted during extraction."),
            "phase15",
            None,
        )
        .await
    });
    entered.notified().await;
    let source_id = chunks(&state, "phase15")[0].id.clone();
    let outcome = crate::handlers::delete::delete_memory_fully(
        &state.store,
        &state.tantivy,
        &state.graph,
        &source_id,
    )
    .await
    .unwrap();
    assert!(outcome.all_ok());
    release.notify_one();
    let result = task.await.unwrap().unwrap();
    assert!(result.content[0].text.contains(&source_id));
    assert!(chunks(&state, "phase15").is_empty());
    assert!(state
        .store
        .get_chunk(&source_id)
        .unwrap()
        .unwrap()
        .deleted_at
        .is_some());
    assert_eq!(
        get(&app, &source_id).await.0,
        axum::http::StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn partial_extraction_keeps_complete_source_and_only_linked_facts() {
    let (_, state) = fixture(5);
    let chat = Chat::sequence(vec![
        Ok(reply(vec![fact(None)])),
        Ok(ChatHttpReply {
            status: 429,
            body: "synthetic quota".into(),
        }),
    ]);
    let transcript =
        "Synthetic source part one.\n\nSynthetic source part two.\n\nSynthetic source part three.";
    let result = tool_ingest_with(&chat, &state, args(transcript), "phase15", None)
        .await
        .unwrap();
    let records = chunks(&state, "phase15");
    let source = records
        .iter()
        .find(|chunk| kind(chunk) == "source")
        .unwrap();
    assert_eq!(source.content, transcript);
    let derived: Vec<_> = records
        .iter()
        .filter(|chunk| kind(chunk) == "fact")
        .collect();
    assert_eq!(derived.len(), 1);
    assert_link(derived[0], source);
    assert!(chat.calls() > 1);
    assert!(result.content[0].text.contains(&source.id));
    assert!(
        result.content[0].text.to_lowercase().contains("incomplete")
            || result.content[0].text.to_lowercase().contains("partial")
    );
}

struct FailWrite {
    fail_at: usize,
    writes: std::sync::atomic::AtomicUsize,
}

impl IngestPersistence for FailWrite {
    async fn persist(
        &self,
        state: &Arc<crate::AppState>,
        chunk: loomem_core::storage::Chunk,
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
async fn source_persistence_failure_blocks_chat_and_facts() {
    let (_, state) = fixture(20000);
    let chat = Chat::facts(vec![fact(None)]);
    let sink = FailWrite {
        fail_at: 1,
        writes: std::sync::atomic::AtomicUsize::new(0),
    };
    let result = tool_ingest_with_sink(
        &chat,
        &sink,
        &state,
        args("Synthetic source write failure."),
        "phase15",
        None,
    )
    .await
    .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert_eq!(chat.calls(), 0);
    assert!(chunks(&state, "phase15").is_empty());
    assert!(result.content[0].text.contains("Source persistence failed"));
    assert!(!result.content[0].text.contains("Source stored"));
}

#[tokio::test]
async fn failed_derived_write_keeps_source_and_counts_only_persisted_facts() {
    let (_, state) = fixture(20000);
    let mut second = fact(None);
    second["content"] = json!("The user prefers a flexible cancellation policy.");
    let chat = Chat::facts(vec![fact(None), second]);
    let sink = FailWrite {
        fail_at: 3,
        writes: std::sync::atomic::AtomicUsize::new(0),
    };
    let result = tool_ingest_with_sink(
        &chat,
        &sink,
        &state,
        args("Synthetic source for two preferences."),
        "phase15",
        None,
    )
    .await
    .unwrap();
    let records = chunks(&state, "phase15");
    assert_eq!(records.len(), 2);
    let source = records
        .iter()
        .find(|chunk| kind(chunk) == "source")
        .unwrap();
    let derived = records.iter().find(|chunk| kind(chunk) == "fact").unwrap();
    assert_link(derived, source);
    assert!(result.content[0].text.contains(&source.id));
    assert!(result.content[0].text.contains("stored 1"));
    assert!(result.content[0].text.contains("skipped 1"));
}

#[tokio::test]
async fn malformed_model_date_does_not_become_event_or_validity_date() {
    let (_, state) = fixture(20000);
    let chat = Chat::facts(vec![fact(Some("not-a-date"))]);
    tool_ingest_with(
        &chat,
        &state,
        args("Synthetic dateless source."),
        "phase15",
        None,
    )
    .await
    .unwrap();
    let records = chunks(&state, "phase15");
    let derived = records.iter().find(|chunk| kind(chunk) == "fact").unwrap();
    assert_eq!(derived.extraction_meta.as_ref().unwrap().event_date, None);
    assert_eq!(derived.valid_from, None);
}

#[tokio::test]
async fn disabled_extraction_stores_canonical_source_without_calling_chat() {
    let (_, state) = crate::tests::make_test_app();
    assert!(!state.config.knowledge_extraction.enabled);
    let input = json!({"content":"user: Synthetic source without extraction.", "stream":"outside"});
    let result = tool_ingest(&state, input, "phase15", None).await.unwrap();
    let records = chunks(&state, "phase15");
    assert_eq!(records.len(), 1);
    assert_eq!(state.store.get_all_chunks().unwrap().len(), 1);
    assert_eq!(kind(&records[0]), "source");
    assert_eq!(records[0].valid_from, None);
    assert_eq!(
        records[0].metadata.as_ref().unwrap()["_loomem_ingest"]["conversation_date"],
        serde_json::Value::Null
    );
    assert!(result.content[0].text.contains(&records[0].id));
    assert_ne!(result.is_error, Some(true));
    assert!(result.content[0].text.contains("extraction disabled"));
}

#[tokio::test]
async fn pre_epoch_event_date_keeps_date_without_fabricating_unsigned_validity() {
    let (_, state) = fixture(20000);
    let chat = Chat::facts(vec![fact(Some("1956-06-15"))]);
    tool_ingest_with(
        &chat,
        &state,
        args("Synthetic event with explicitly stated historical date."),
        "phase15",
        None,
    )
    .await
    .unwrap();
    let records = chunks(&state, "phase15");
    let derived = records.iter().find(|chunk| kind(chunk) == "fact").unwrap();
    assert_eq!(
        derived
            .extraction_meta
            .as_ref()
            .unwrap()
            .event_date
            .as_deref(),
        Some("1956-06-15")
    );
    assert_eq!(derived.valid_from, None);
}

#[tokio::test]
async fn missing_conversation_date_sends_unknown_reference_without_resolving_relative_time() {
    let (_, state) = fixture(20000);
    let chat = Chat::facts(vec![fact(None)]);
    let input = json!({"content":"user: I went to the synthetic workshop yesterday."});
    tool_ingest_with(&chat, &state, input, "phase15", None)
        .await
        .unwrap();
    let requests = chat.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let system = requests[0]["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("The conversation_date is: unknown"));
    let records = chunks(&state, "phase15");
    let source = records
        .iter()
        .find(|chunk| kind(chunk) == "source")
        .unwrap();
    let derived = records.iter().find(|chunk| kind(chunk) == "fact").unwrap();
    assert_eq!(
        source.metadata.as_ref().unwrap()["_loomem_ingest"]["conversation_date"],
        serde_json::Value::Null
    );
    assert_eq!(source.valid_from, None);
    assert_eq!(derived.extraction_meta.as_ref().unwrap().event_date, None);
    assert_eq!(derived.valid_from, None);
}

#[tokio::test]
async fn explicit_model_event_date_survives_without_conversation_anchor() {
    let (_, state) = fixture(20000);
    let chat = Chat::facts(vec![fact(Some("2024-03-19"))]);
    let input = json!({"content":"user: I attended the synthetic workshop on 2024-03-19."});
    tool_ingest_with(&chat, &state, input, "phase15", None)
        .await
        .unwrap();
    let records = chunks(&state, "phase15");
    let derived = records.iter().find(|chunk| kind(chunk) == "fact").unwrap();
    let meta = derived.extraction_meta.as_ref().unwrap();
    assert_eq!(meta.event_date.as_deref(), Some("2024-03-19"));
    assert_eq!(derived.valid_from, meta.event_date_unix());
    assert_eq!(
        derived.metadata.as_ref().unwrap()["_loomem_ingest"]["conversation_date"],
        serde_json::Value::Null
    );
}

#[tokio::test]
async fn fact_only_mcp_delete_names_retained_source_and_preserves_sibling() {
    let (app, state) = fixture(20000);
    let mut second = fact(None);
    second["content"] = json!("The user prefers a flexible cancellation policy.");
    let chat = Chat::facts(vec![fact(None), second]);
    let transcript =
        "user: Source record of preferences: quiet accommodation and flexible cancellation.";
    tool_ingest_with(&chat, &state, args(transcript), "phase15", None)
        .await
        .unwrap();
    let records = chunks(&state, "phase15");
    let source = records
        .iter()
        .find(|chunk| kind(chunk) == "source")
        .unwrap();
    let target = records
        .iter()
        .find(|chunk| kind(chunk) == "fact" && chunk.content.contains("quiet"))
        .unwrap();
    let sibling = records
        .iter()
        .find(|chunk| kind(chunk) == "fact" && chunk.content.contains("cancellation"))
        .unwrap();
    let result = super::super::tool_delete(&state, json!({"chunk_id":target.id}), "phase15")
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    assert!(result.content[0].text.contains(&source.id));
    assert!(state
        .store
        .get_chunk(&target.id)
        .unwrap()
        .unwrap()
        .deleted_at
        .is_some());
    assert!(state
        .store
        .get_chunk(&source.id)
        .unwrap()
        .unwrap()
        .deleted_at
        .is_none());
    assert!(state
        .store
        .get_chunk(&sibling.id)
        .unwrap()
        .unwrap()
        .deleted_at
        .is_none());
    assert_link(
        &state.store.get_chunk(&sibling.id).unwrap().unwrap(),
        source,
    );
    assert_eq!(
        get(&app, &target.id).await.0,
        axum::http::StatusCode::NOT_FOUND
    );
    assert_eq!(get(&app, &source.id).await.0, axum::http::StatusCode::OK);
    assert_eq!(get(&app, &sibling.id).await.0, axum::http::StatusCode::OK);
    let index = state.tantivy.lock().await;
    let source_hits = index
        .search_with_stream("Source record", "phase15", 10)
        .unwrap();
    assert!(source_hits.iter().any(|hit| hit.id == source.id));
    let fact_hits = index
        .search_with_stream("cancellation", "phase15", 10)
        .unwrap();
    assert!(fact_hits.iter().any(|hit| hit.id == sibling.id));
    let deleted_hits = index.search_with_stream("quiet", "phase15", 10).unwrap();
    assert!(!deleted_hits.iter().any(|hit| hit.id == target.id));
}

async fn cached_search(app: &axum::Router) -> serde_json::Value {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/search")
        .header("Authorization", "Bearer admin-token")
        .header("Content-Type", "application/json")
        .body(axum::body::Body::from(
            json!({"query":"quiet accommodation", "streams":["phase15"], "top_k":10}).to_string(),
        ))
        .unwrap();
    let (status, bytes) = crate::tests::send(app.clone(), request).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn warmed_search_cache_cannot_return_source_or_facts_after_source_cascade() {
    let (_, original) = fixture(20000);
    let mut config = original.config.clone();
    config.search.cache.enabled = true;
    config.search.decay.l0_lambda = 0.0;
    config.search.decay.l1_lambda = 0.0;
    let (app, state) = crate::tests::make_test_app_cfg(config);
    let chat = Chat::facts(vec![fact(None)]);
    tool_ingest_with(
        &chat,
        &state,
        args("user: I prefer quiet accommodation."),
        "phase15",
        None,
    )
    .await
    .unwrap();
    let records = chunks(&state, "phase15");
    let source = records
        .iter()
        .find(|chunk| kind(chunk) == "source")
        .unwrap();
    let cold = cached_search(&app).await;
    assert!(!cold["results"].as_array().unwrap().is_empty());
    assert_eq!(state.query_cache.lock().await.len(), 1);
    let warm = cached_search(&app).await;
    assert!(warm["results"]
        .as_array()
        .unwrap()
        .iter()
        .all(|record| record["metadata"]["cached"] == true));
    assert!(warm["results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|record| record["id"] == source.id));
    let outcome = crate::handlers::delete::delete_memory_fully(
        &state.store,
        &state.tantivy,
        &state.graph,
        &source.id,
    )
    .await
    .unwrap();
    assert!(outcome.all_ok());
    assert_eq!(
        state.query_cache.lock().await.len(),
        1,
        "proof must exercise stale cache membership"
    );
    let deleted = cached_search(&app).await;
    assert!(deleted["results"].as_array().unwrap().is_empty());
    assert!(chunks(&state, "phase15").is_empty());
}

#[path = "date_render_tests.rs"]
mod date_render_tests;

#[tokio::test]
async fn truncated_completion_keeps_source_without_admitting_valid_json_prefix() {
    for content in [
        json!({"facts":[fact(None)]}).to_string(),
        "{\"facts\":[".into(),
    ] {
        let (_, state) = fixture(20000);
        let chat = Chat::sequence(vec![Ok(ChatHttpReply {
            status: 200,
            body: json!({"choices":[{"finish_reason":"length","message":{"content":content}}]})
                .to_string(),
        })]);
        let result = tool_ingest_with(
            &chat,
            &state,
            args("user: Synthetic truncation source."),
            "phase18",
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.is_error, Some(true));
        assert!(result.content[0]
            .text
            .contains("output token limit reached"));
        assert!(!result.content[0].text.contains("503"));
        let records = chunks(&state, "phase18");
        assert_eq!(records.len(), 1);
        let source = &records[0];
        assert_eq!(kind(source), "source");
        assert!(result.content[0].text.contains(&source.id));
        assert_eq!(chat.calls(), 1);
        let indexed = state
            .tantivy
            .lock()
            .await
            .search_with_stream("truncation source", "phase18", 10)
            .unwrap();
        assert!(indexed.iter().any(|hit| hit.id == source.id));
    }
}

#[tokio::test]
async fn truncated_later_chunk_reports_partial_extraction_without_extra_facts() {
    let (_, state) = fixture(10);
    let limited = ChatHttpReply {
        status: 200,
        body: json!({"choices":[{"finish_reason":"length","message":{"content":json!({"facts":[fact(None)]}).to_string()}}]}).to_string(),
    };
    let chat = Chat::sequence(vec![Ok(reply(vec![fact(None)])), Ok(limited)]);
    let result = tool_ingest_with(&chat, &state, args(&"x".repeat(60)), "phase18", None)
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    assert!(result.content[0].text.contains("Extracted 1 facts"));
    assert!(result.content[0]
        .text
        .contains("Warning: 1 extraction chunk(s) failed"));
    assert!(result.content[0]
        .text
        .contains("output token limit reached"));
    assert_eq!(chat.calls(), 2);
    let records = chunks(&state, "phase18");
    assert_eq!(records.len(), 2);
    let source = records.iter().find(|row| kind(row) == "source").unwrap();
    let derived = records.iter().find(|row| kind(row) == "fact").unwrap();
    assert_link(derived, source);
}
