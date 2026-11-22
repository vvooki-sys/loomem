//! Evaluation-only expected-write evidence, never an index/vector readiness claim.
//! Expectations are captured before persistence; no storage reads establish them.
use super::{fact_chunk, ExtractionOutcome, IngestContext, IngestPersistence, ToolResult};
#[cfg(any(test, feature = "evaluation"))]
use loomem_core::llm_provider::ProviderError;
use loomem_core::memory_extractor::ExtractionError;
#[cfg(any(test, feature = "evaluation"))]
use serde_json::{Map, Value};

#[cfg(any(test, feature = "evaluation"))]
mod projection;
#[cfg(any(test, feature = "evaluation"))]
pub(crate) use projection::ExpectedFact;

#[derive(Default)]
pub(super) struct Writes {
    pub stored: usize,
    pub failed: usize,
    #[cfg(any(test, feature = "evaluation"))]
    identity_mismatches: usize,
    #[cfg(any(test, feature = "evaluation"))]
    invalid_expectations: usize,
    #[cfg(any(test, feature = "evaluation"))]
    facts: Vec<Value>,
}

pub(super) async fn persist_facts(
    context: &IngestContext<'_>,
    sink: &impl IngestPersistence,
    outcome: &ExtractionOutcome,
) -> Writes {
    let mut writes = Writes::default();
    for (ordinal, fact) in outcome.facts.iter().enumerate() {
        let candidate = fact_chunk(context, fact);
        let pending = Pending::capture(context, &candidate, ordinal);
        // Pass the original candidate: passing the projected text would add
        // another sanitizer pass and could change encoded text again.
        match sink.persist(context.state, candidate).await {
            Ok(id) => writes.success(pending, id),
            Err(_) => writes.failed += 1,
        }
    }
    writes
}

struct Pending {
    #[cfg(any(test, feature = "evaluation"))]
    ordinal: usize,
    #[cfg(any(test, feature = "evaluation"))]
    allocated_id: String,
    #[cfg(any(test, feature = "evaluation"))]
    expected: Option<ExpectedFact>,
}

impl Pending {
    fn capture(context: &IngestContext<'_>, candidate: &super::Chunk, ordinal: usize) -> Self {
        #[cfg(any(test, feature = "evaluation"))]
        {
            Self {
                ordinal,
                allocated_id: candidate.id.clone(),
                expected: ExpectedFact::capture(context, candidate),
            }
        }
        #[cfg(not(any(test, feature = "evaluation")))]
        {
            let _ = (context, candidate, ordinal);
            Self {}
        }
    }
}

impl Writes {
    fn success(&mut self, pending: Pending, id: String) {
        self.stored += 1;
        #[cfg(any(test, feature = "evaluation"))]
        {
            self.identity_mismatches += usize::from(id != pending.allocated_id);
            self.invalid_expectations += usize::from(pending.expected.is_none());
            self.facts.push(serde_json::json!({
                "accepted_ordinal": pending.ordinal, "id": id,
                "allocated_id": pending.allocated_id, "expected": pending.expected,
            }));
        }
        #[cfg(not(any(test, feature = "evaluation")))]
        let _ = (pending, id);
    }

    pub(super) fn attach(
        self,
        result: ToolResult,
        source_id: &str,
        outcome: &ExtractionOutcome,
    ) -> ToolResult {
        #[cfg(any(test, feature = "evaluation"))]
        {
            let status = if extraction_complete(outcome) && self.confirmed() {
                "complete"
            } else {
                "partial"
            };
            attach(
                result,
                source_id,
                status,
                Outcome::completed(outcome),
                self,
                None,
            )
        }
        #[cfg(not(any(test, feature = "evaluation")))]
        {
            let _ = (self, source_id, outcome);
            result
        }
    }

    #[cfg(any(test, feature = "evaluation"))]
    fn confirmed(&self) -> bool {
        self.failed == 0 && self.identity_mismatches == 0 && self.invalid_expectations == 0
    }
}

pub(super) fn unavailable(
    result: ToolResult,
    source_id: &str,
    error: Option<&ExtractionError>,
) -> ToolResult {
    #[cfg(any(test, feature = "evaluation"))]
    {
        let (status, outcome, code) = match error {
            Some(error) => ("failed", Outcome::failed(error), Some(error_code(error))),
            None => ("disabled", Outcome::default(), None),
        };
        attach(result, source_id, status, outcome, Writes::default(), code)
    }
    #[cfg(not(any(test, feature = "evaluation")))]
    {
        let _ = (source_id, error);
        result
    }
}

/// Preserve additive sibling receipts while leaving human text/error untouched.
#[cfg(any(test, feature = "evaluation"))]
pub(super) fn insert(result: &mut ToolResult, key: &str, value: Value) {
    let mut fields = match result.structured_content.take() {
        Some(Value::Object(fields)) => fields,
        _ => Map::new(),
    };
    fields.insert(key.to_owned(), value);
    result.structured_content = Some(Value::Object(fields));
}

#[cfg(any(test, feature = "evaluation"))]
fn extraction_complete(outcome: &ExtractionOutcome) -> bool {
    outcome.failures.is_empty()
        && outcome.input_chunks_processed == outcome.input_chunks_total
        && outcome.discarded_facts == 0
}

#[cfg(any(test, feature = "evaluation"))]
#[derive(Default, serde::Serialize)]
struct Outcome {
    input_chunks_total: Option<usize>,
    input_chunks_processed: Option<usize>,
    calls_attempted: Option<usize>,
    calls_completed: Option<usize>,
    calls_truncated: Option<usize>,
    failed_attempts: Option<usize>,
    fact_limit_reached: Option<bool>,
    discarded_facts: Option<usize>,
    accepted_facts: Option<usize>,
}

#[cfg(any(test, feature = "evaluation"))]
impl Outcome {
    fn completed(outcome: &ExtractionOutcome) -> Self {
        Self {
            input_chunks_total: Some(outcome.input_chunks_total),
            input_chunks_processed: Some(outcome.input_chunks_processed),
            calls_attempted: Some(outcome.chunks_attempted),
            calls_completed: Some(outcome.chunks_completed),
            calls_truncated: Some(outcome.chunks_truncated),
            failed_attempts: Some(outcome.failures.len()),
            fact_limit_reached: Some(outcome.fact_limit_reached),
            discarded_facts: Some(outcome.discarded_facts),
            accepted_facts: Some(outcome.facts.len()),
        }
    }

    fn failed(error: &ExtractionError) -> Self {
        match error {
            ExtractionError::AllChunksFailed {
                chunks_attempted,
                failures,
            } => Self {
                calls_attempted: Some(*chunks_attempted),
                calls_completed: Some(0),
                failed_attempts: Some(failures.len()),
                accepted_facts: Some(0),
                ..Self::default()
            },
            ExtractionError::Provider(_) | ExtractionError::InvalidConfig(_) => Self {
                calls_attempted: Some(0),
                calls_completed: Some(0),
                calls_truncated: Some(0),
                failed_attempts: Some(0),
                accepted_facts: Some(0),
                ..Self::default()
            },
        }
    }
}

#[cfg(any(test, feature = "evaluation"))]
fn error_code(error: &ExtractionError) -> &'static str {
    match error {
        ExtractionError::Provider(ProviderError::MissingKey { .. }) => "missing_api_key",
        ExtractionError::Provider(ProviderError::Unknown(_)) => "unknown_provider",
        ExtractionError::InvalidConfig(_) => "invalid_config",
        ExtractionError::AllChunksFailed { .. } => "all_chunks_failed",
    }
}

#[cfg(any(test, feature = "evaluation"))]
fn attach(
    mut result: ToolResult,
    source_id: &str,
    status: &str,
    outcome: Outcome,
    writes: Writes,
    error_code: Option<&str>,
) -> ToolResult {
    insert(
        &mut result,
        "extraction_receipt",
        serde_json::json!({
            "version": 1, "scope": "generated_facts",
            "evidence": "pre_persistence_expected_records_v1", "source_id": source_id,
            "status": status, "outcome": outcome, "error_code": error_code,
            "persistence": {"stored": writes.stored, "failed": writes.failed,
                "identity_mismatches": writes.identity_mismatches,
                "invalid_expectations": writes.invalid_expectations},
            "facts": writes.facts,
        }),
    );
    result
}

#[cfg(test)]
mod tests;
