use super::super::{
    fixtures::*, prepare, tool_ingest, tool_ingest_with, tool_ingest_with_sink, FixedClock,
    RealPersistence,
};
use super::*;
use loomem_core::memory_extractor::{ChatHttpReply, ExtractedFact};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

fn receipt(result: &ToolResult) -> &Value {
    &result.structured_content.as_ref().unwrap()["extraction_receipt"]
}

fn configured(change: impl FnOnce(&mut loomem_core::Config)) -> Arc<crate::AppState> {
    let (_, original) = fixture(20000);
    let mut config = original.config.clone();
    change(&mut config);
    crate::tests::make_test_app_cfg(config).1
}

#[tokio::test]
async fn actual_ids_and_sanitized_projections_match_real_storage_and_preserve_sibling() {
    let state = configured(|config| {
        config.fragments.enabled = true;
        config.fragments.token_counter = loomem_core::fragments::TokenCounterKind::Approx;
    });
    let mut input = fact(None);
    let raw = "&amp;amp;amp;amp;amp;amp;lt;b&amp;amp;amp;amp;amp;amp;gt;quiet&amp;amp;amp;amp;amp;amp;lt;/b&amp;amp;amp;amp;amp;amp;gt; synthetic-person@example.invalid";
    input["content"] = json!(raw);
    input["subject"] = json!("<b>synthetic-person@example.invalid</b>");
    let first = state.pii_filter.redact_for_sink(raw);
    let second = crate::handlers::ingest::sanitize::stored_content(&state.pii_filter, &first);
    let third = crate::handlers::ingest::sanitize::stored_content(&state.pii_filter, &second);
    assert_ne!(
        second, third,
        "fixture must expose an accidental third pass"
    );
    let result = tool_ingest_with(
        &Chat::facts(vec![input]),
        &state,
        args("user: I prefer quiet accommodation."),
        "receipt",
        Some("private-authenticated-user".into()),
    )
    .await
    .unwrap();
    let receipt = receipt(&result);
    assert_eq!(receipt["status"], "complete");
    assert_eq!(receipt["evidence"], "pre_persistence_expected_records_v1");
    assert_eq!(receipt["outcome"]["accepted_facts"], 1);
    assert_eq!(receipt["persistence"]["stored"], 1);
    let row = &receipt["facts"][0];
    assert_eq!(row["accepted_ordinal"], 0);
    assert_eq!(row["id"], row["allocated_id"]);
    let chunk = state
        .store
        .get_chunk(row["id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    let stored = serde_json::to_value(&chunk).unwrap();
    assert_eq!(row["expected"]["content"], second);
    for (field, value) in row["expected"].as_object().unwrap() {
        assert_eq!(
            stored.get(field),
            Some(value),
            "projection mismatch: {field}"
        );
    }
    assert_eq!(row["expected"]["provenance_role"], "Claim");
    assert_eq!(row["expected"]["source_ids"], json!([receipt["source_id"]]));
    assert_eq!(
        row["expected"]["extraction_meta"]["extracted_from"],
        receipt["source_id"]
    );
    let serialized = serde_json::to_string(receipt).unwrap();
    assert!(!serialized.contains("synthetic-person@example.invalid"));
    assert!(!serialized.contains("private-authenticated-user"));
    for field in [
        "stream",
        "ingester_user_id",
        "created_by",
        "access_count",
        "alpha",
        "beta",
    ] {
        assert!(
            row["expected"].get(field).is_none(),
            "unexpected private field: {field}"
        );
    }
    let source_receipt = &result.structured_content.as_ref().unwrap()["ingest_receipt"];
    assert_eq!(source_receipt["source_id"], receipt["source_id"]);
    assert_eq!(source_receipt["status"], "complete");
    assert!(!source_receipt["fragment_ids"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_ne!(result.is_error, Some(true));
}

#[tokio::test]
async fn disabled_missing_key_and_successful_empty_extraction_are_distinct() {
    let disabled = configured(|config| config.knowledge_extraction.enabled = false);
    let result = tool_ingest(&disabled, args("user: A source."), "receipt", None)
        .await
        .unwrap();
    assert_eq!(receipt(&result)["status"], "disabled");
    assert!(receipt(&result)["outcome"]["calls_attempted"].is_null());
    assert_eq!(receipt(&result)["facts"], json!([]));
    let state = configured(|config| {
        config.llm.api_key = None;
        config.llm.api_key_env = String::new();
    });
    let result = tool_ingest(&state, args("user: A source."), "receipt", None)
        .await
        .unwrap();
    assert_eq!(receipt(&result)["status"], "failed");
    assert_eq!(receipt(&result)["error_code"], "missing_api_key");
    assert_eq!(receipt(&result)["outcome"]["calls_attempted"], 0);
    assert!(receipt(&result)["outcome"]["input_chunks_total"].is_null());
    assert_eq!(result.is_error, Some(true));
    let result = tool_ingest_with(
        &Chat::facts(vec![]),
        &state,
        args("user: A source."),
        "receipt",
        None,
    )
    .await
    .unwrap();
    assert_eq!(receipt(&result)["status"], "complete");
    assert_eq!(receipt(&result)["outcome"]["accepted_facts"], 0);
    assert_eq!(receipt(&result)["outcome"]["input_chunks_processed"], 1);
    assert_eq!(receipt(&result)["outcome"]["calls_completed"], 1);
    assert_eq!(receipt(&result)["facts"], json!([]));
    assert_ne!(result.is_error, Some(true));
}

#[tokio::test]
async fn unknown_provider_is_reported_with_its_own_error_code() {
    assert_eq!(
        error_code(&ExtractionError::Provider(ProviderError::Unknown(
            "nope".into()
        ))),
        "unknown_provider"
    );
    let state = configured(|config| config.llm.provider = "nope".into());
    let result = tool_ingest(&state, args("user: A source."), "receipt", None)
        .await
        .unwrap();
    assert_eq!(receipt(&result)["status"], "failed");
    assert_eq!(receipt(&result)["error_code"], "unknown_provider");
    assert_eq!(receipt(&result)["outcome"]["calls_attempted"], 0);
    assert!(receipt(&result)["outcome"]["input_chunks_total"].is_null());
    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn all_failed_calls_preserve_error_and_unknown_input_counts() {
    let (_, state) = fixture(20000);
    let chat = Chat::sequence(vec![Err(anyhow::anyhow!("synthetic transport failure"))]);
    let result = tool_ingest_with(&chat, &state, args("user: A source."), "receipt", None)
        .await
        .unwrap();
    let receipt = receipt(&result);
    assert_eq!(result.is_error, Some(true));
    assert_eq!(receipt["status"], "failed");
    assert_eq!(receipt["error_code"], "all_chunks_failed");
    assert_eq!(receipt["outcome"]["calls_attempted"], 1);
    assert_eq!(receipt["outcome"]["failed_attempts"], 1);
    assert!(receipt["outcome"]["input_chunks_processed"].is_null());
    assert!(receipt["outcome"]["calls_truncated"].is_null());
    assert_eq!(receipt["persistence"]["stored"], 0);
    assert_eq!(
        result.structured_content.as_ref().unwrap()["ingest_receipt"]["status"],
        "complete"
    );
}

#[tokio::test]
async fn partial_extraction_does_not_claim_full_completion() {
    let (_, state) = fixture(10);
    let chat = Chat::sequence(vec![
        Ok(reply(vec![fact(None)])),
        Err(anyhow::anyhow!("later failure")),
    ]);
    let result = tool_ingest_with(&chat, &state, args(&"x".repeat(60)), "receipt", None)
        .await
        .unwrap();
    assert_eq!(receipt(&result)["status"], "partial");
    assert_eq!(receipt(&result)["outcome"]["input_chunks_processed"], 2);
    assert_eq!(receipt(&result)["outcome"]["failed_attempts"], 1);
    assert_eq!(receipt(&result)["persistence"]["stored"], 1);
}

struct InterceptSink {
    attempts: AtomicUsize,
    fail_ordinal: Option<usize>,
    mismatch_ordinal: Option<usize>,
}

impl IngestPersistence for InterceptSink {
    async fn persist(
        &self,
        state: &Arc<crate::AppState>,
        chunk: super::super::Chunk,
    ) -> Result<String, crate::handlers::AppError> {
        if kind(&chunk) != "fact" {
            return RealPersistence.persist(state, chunk).await;
        }
        let ordinal = self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.fail_ordinal == Some(ordinal) {
            return Err(crate::handlers::AppError::Internal(anyhow::anyhow!(
                "synthetic failure"
            )));
        }
        let id = RealPersistence.persist(state, chunk).await?;
        Ok(if self.mismatch_ordinal == Some(ordinal) {
            "00000000-0000-4000-8000-000000000099".into()
        } else {
            id
        })
    }
}

#[tokio::test]
async fn partial_persistence_preserves_sparse_accepted_ordinals() {
    let (_, state) = fixture(20000);
    let sink = InterceptSink {
        attempts: AtomicUsize::new(0),
        fail_ordinal: Some(1),
        mismatch_ordinal: None,
    };
    let chat = Chat::facts(vec![fact(None), fact(None), fact(None)]);
    let result = tool_ingest_with_sink(
        &chat,
        &sink,
        &state,
        args("user: A source."),
        "receipt",
        None,
    )
    .await
    .unwrap();
    let receipt = receipt(&result);
    assert_eq!(receipt["status"], "partial");
    assert_eq!(receipt["outcome"]["accepted_facts"], 3);
    assert_eq!(receipt["persistence"]["stored"], 2);
    assert_eq!(receipt["persistence"]["failed"], 1);
    assert_eq!(receipt["facts"][0]["accepted_ordinal"], 0);
    assert_eq!(receipt["facts"][1]["accepted_ordinal"], 2);
    for row in receipt["facts"].as_array().unwrap() {
        assert!(state
            .store
            .get_chunk(row["id"].as_str().unwrap())
            .unwrap()
            .is_some());
    }
}

#[tokio::test]
async fn mismatched_returned_id_stays_diagnostic_and_never_rebinds_expectations() {
    let (_, state) = fixture(20000);
    let sink = InterceptSink {
        attempts: AtomicUsize::new(0),
        fail_ordinal: None,
        mismatch_ordinal: Some(0),
    };
    let result = tool_ingest_with_sink(
        &Chat::facts(vec![fact(None)]),
        &sink,
        &state,
        args("user: A source."),
        "receipt",
        None,
    )
    .await
    .unwrap();
    let receipt = receipt(&result);
    assert_eq!(receipt["status"], "partial");
    assert_eq!(receipt["persistence"]["identity_mismatches"], 1);
    let row = &receipt["facts"][0];
    assert_eq!(row["id"], "00000000-0000-4000-8000-000000000099");
    assert_ne!(row["id"], row["allocated_id"]);
    let allocated = state
        .store
        .get_chunk(row["allocated_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(row["expected"]["content"], allocated.content);
    assert!(state
        .store
        .get_chunk(row["id"].as_str().unwrap())
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn final_exact_cap_is_complete_but_discarded_facts_are_partial() {
    let state = configured(|config| config.knowledge_extraction.max_facts_per_chunk = 1);
    for (facts, status, discarded) in [
        (vec![fact(None)], "complete", 0),
        (vec![fact(None), fact(None)], "partial", 1),
    ] {
        let result = tool_ingest_with(
            &Chat::facts(facts),
            &state,
            args("user: A source."),
            "receipt",
            None,
        )
        .await
        .unwrap();
        let receipt = receipt(&result);
        assert_eq!(receipt["status"], status);
        assert_eq!(receipt["outcome"]["fact_limit_reached"], true);
        assert_eq!(receipt["outcome"]["discarded_facts"], discarded);
        assert_eq!(receipt["outcome"]["input_chunks_processed"], 1);
    }
}

/// The cap is per chunk: a chunk that overflows it is truncated and
/// counted, and the chunks after it are still extracted.
#[tokio::test]
async fn per_chunk_cap_processes_every_original_chunk() {
    let state = configured(|config| {
        config.knowledge_extraction.input_chunk_tokens = 4;
        config.knowledge_extraction.max_facts_per_chunk = 1;
    });
    let mut second = fact(None);
    second["content"] = json!("The user books a window seat.");
    let chat = Chat::sequence(vec![
        Ok(reply(vec![fact(None), fact(None)])),
        Ok(reply(vec![second])),
    ]);
    let result = tool_ingest_with(
        &chat,
        &state,
        args("l1 aaaa\nl2 bbbb\nl3 cccc\nl4 dddd"),
        "receipt",
        None,
    )
    .await
    .unwrap();
    assert_eq!(receipt(&result)["status"], "partial");
    assert_eq!(receipt(&result)["outcome"]["input_chunks_total"], 2);
    assert_eq!(receipt(&result)["outcome"]["input_chunks_processed"], 2);
    assert_eq!(receipt(&result)["outcome"]["fact_limit_reached"], true);
    assert_eq!(receipt(&result)["outcome"]["discarded_facts"], 1);
    assert_eq!(receipt(&result)["outcome"]["accepted_facts"], 2);
    assert_eq!(chat.calls(), 2);
}

#[tokio::test]
async fn recovered_truncation_is_complete_with_separate_call_counters() {
    let state = configured(|config| config.knowledge_extraction.split_on_truncation = true);
    let truncated = ChatHttpReply {
        status: 200,
        body: json!({"choices":[{
        "finish_reason":"length", "message":{"content":"{\"facts\":[]}"}}]})
        .to_string(),
    };
    let chat = Chat::sequence(vec![
        Ok(truncated),
        Ok(reply(vec![fact(None)])),
        Ok(reply(vec![fact(None)])),
    ]);
    let result = tool_ingest_with(
        &chat,
        &state,
        args("l1 aaaa\nl2 bbbb\nl3 cccc\nl4 dddd"),
        "receipt",
        None,
    )
    .await
    .unwrap();
    let receipt = receipt(&result);
    assert_eq!(receipt["status"], "complete");
    assert_eq!(receipt["outcome"]["input_chunks_processed"], 1);
    assert_eq!(receipt["outcome"]["calls_attempted"], 3);
    assert_eq!(receipt["outcome"]["calls_completed"], 2);
    assert_eq!(receipt["outcome"]["calls_truncated"], 1);
    assert_eq!(receipt["outcome"]["failed_attempts"], 0);
}

#[test]
fn invalid_numeric_expectations_cannot_serialize_as_complete() {
    let (_, state) = fixture(20000);
    let context = prepare(
        &state,
        args("user: A source."),
        "receipt",
        None,
        &FixedClock,
    )
    .unwrap();
    let fact: ExtractedFact = serde_json::from_value(fact(None)).unwrap();
    for field in ["score", "importance", "confidence"] {
        let mut candidate = fact_chunk(&context, &fact);
        match field {
            "score" => candidate.score = f64::NAN,
            "importance" => candidate.importance = Some(f64::INFINITY),
            _ => candidate.extraction_meta.as_mut().unwrap().confidence = f64::NAN,
        }
        let pending = Pending::capture(&context, &candidate, 0);
        assert!(pending.expected.is_none());
        let mut writes = Writes::default();
        writes.success(pending, candidate.id);
        let outcome = ExtractionOutcome {
            facts: vec![fact.clone()],
            input_chunks_total: 1,
            input_chunks_processed: 1,
            chunks_attempted: 1,
            chunks_completed: 1,
            ..ExtractionOutcome::default()
        };
        let result = writes.attach(ToolResult::text("unchanged"), &context.source_id, &outcome);
        assert_eq!(receipt(&result)["status"], "partial");
        assert_eq!(receipt(&result)["persistence"]["invalid_expectations"], 1);
        assert!(receipt(&result)["facts"][0]["expected"].is_null());
    }
}

#[test]
fn projection_rejects_unbound_source_without_reading_storage() {
    let (_, state) = fixture(20000);
    let context = prepare(
        &state,
        args("user: A source."),
        "receipt",
        None,
        &FixedClock,
    )
    .unwrap();
    let fact: ExtractedFact = serde_json::from_value(fact(None)).unwrap();
    let mut candidate = fact_chunk(&context, &fact);
    assert!(ExpectedFact::capture(&context, &candidate).is_some());
    assert!(state.store.get_chunk(&candidate.id).unwrap().is_none());
    candidate.source_ids = Some(vec!["foreign-source".into()]);
    assert!(ExpectedFact::capture(&context, &candidate).is_none());
}

/// Tripwire, on purpose: `fixtures/complete.json` is the shared golden
/// receipt. Any new field on `ExtractionMeta`, `Chunk` or the projection
/// changes the serialized receipt, so it must be added to the fixture *and*
/// to `ExpectedFact` in the same change — a silent drift between what the
/// engine stores and what the harness reconstructs is exactly what this
/// test exists to catch.
#[tokio::test]
async fn actual_serialized_receipts_match_shared_golden_fixture() {
    let state = configured(|config| {
        config.knowledge_extraction.model = "receipt-fixture-model".into();
    });
    let result = tool_ingest_with(
        &Chat::facts(vec![fact(None)]),
        &state,
        args("user: I prefer quiet accommodation."),
        "receipt",
        None,
    )
    .await
    .unwrap();
    let structured = result.structured_content.unwrap();
    let source = structured["extraction_receipt"]["source_id"]
        .as_str()
        .unwrap();
    let fact = structured["extraction_receipt"]["facts"][0]["id"]
        .as_str()
        .unwrap();
    let normalized = serde_json::to_string(&structured)
        .unwrap()
        .replace(source, "00000000-0000-4000-8000-000000000001")
        .replace(fact, "00000000-0000-4000-8000-000000000002");
    let actual: Value = serde_json::from_str(&normalized).unwrap();
    let expected: Value = serde_json::from_str(include_str!("fixtures/complete.json")).unwrap();
    assert_eq!(actual, expected);
}
