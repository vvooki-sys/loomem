//! Whole-ingest observation for a fresh, immutable, caller-owned evaluation.
//! Caller must first validate both complete receipts. This proves persisted
//! whitelisted metadata, vector shape and indexed ID/stream/content only;
//! neither indexed temporal fields nor extraction/model correctness is proven.
use super::*;
use crate::mcp::dispatcher::ExpectedFact;
use expected_bytes::ExpectedRecord;
use loomem_core::storage::Chunk;

const EVIDENCE: &str = "fresh_immutable_ingest_records_v1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    schema_version: u32,
    scope: String,
    stream: String,
    source: SourceExpectation,
    facts: Vec<FactExpectation>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FactExpectation {
    accepted_ordinal: usize,
    id: String,
    allocated_id: String,
    expected: Value,
}

enum Record {
    Source(ExpectedRecord),
    Fact(String, Box<ExpectedFact>),
}

impl Record {
    fn id(&self) -> &str {
        match self {
            Self::Source(record) => &record.id,
            Self::Fact(id, _) => id,
        }
    }

    fn content(&self) -> &str {
        match self {
            Self::Source(record) => &record.content,
            Self::Fact(_, fact) => fact.content(),
        }
    }

    fn metadata_matches(&self, chunk: &Chunk, date: Option<&str>) -> bool {
        match self {
            Self::Source(record) => expected_bytes::metadata_matches(record, chunk, date),
            Self::Fact(_, fact) => fact.metadata_matches(chunk),
        }
    }
}

fn reconstruct(state: &AppState, request: &Request) -> Result<Vec<Record>, StatusCode> {
    if !valid_header(request.schema_version, &request.scope)
        || request
            .source
            .fragment_ids
            .len()
            .saturating_add(request.facts.len())
            >= MAX_RECORDS
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let mut records: Vec<_> = expected_bytes::reconstruct(state, &request.source)
        .map_err(|_| StatusCode::BAD_REQUEST)?
        .into_iter()
        .map(Record::Source)
        .collect();
    let mut ids: HashSet<_> = records
        .iter()
        .map(|record| record.id().to_owned())
        .collect();
    for (ordinal, row) in request.facts.iter().enumerate() {
        if row.accepted_ordinal != ordinal
            || row.id != row.allocated_id
            || !uuid::Uuid::parse_str(&row.id).is_ok_and(|id| id.to_string() == row.id)
            || !ids.insert(row.id.clone())
        {
            return Err(StatusCode::BAD_REQUEST);
        }
        let fact = ExpectedFact::parse(
            row.expected.clone(),
            &request.source.source_id,
            request.source.conversation_date.as_deref(),
        )
        .map_err(|_| StatusCode::BAD_REQUEST)?;
        records.push(Record::Fact(row.id.clone(), Box::new(fact)));
    }
    Ok(records)
}

pub(super) async fn observe(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(request): Json<Request>,
) -> Result<Json<Value>, StatusCode> {
    observe_impl(state, auth, request, false).await
}

/// Versioned opt-in response adds sanitized immutable fragment projections.
pub(super) async fn observe_v2(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(request): Json<Request>,
) -> Result<Json<Value>, StatusCode> {
    observe_impl(state, auth, request, true).await
}

async fn observe_impl(
    state: Arc<AppState>,
    auth: AuthContext,
    request: Request,
    include_projection: bool,
) -> Result<Json<Value>, StatusCode> {
    let stream = auth::validate_stream(&auth, Some(&request.stream))?;
    // Extraction is the only additional writer allowed by this protocol. It
    // has already completed synchronously before the harness submits receipts.
    let mut config = state.config.clone();
    config.knowledge_extraction.enabled = false;
    if !super::compatible(&config) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let records = reconstruct(&state, &request)?;
    let expected: Vec<_> = records
        .iter()
        .map(|record| ExpectedDocument {
            id: record.id(),
            content: record.content(),
            stream: &stream,
        })
        .collect();
    let index = state.tantivy.lock().await;
    let report = probe_expected_records_with_metadata(
        &state.store,
        &index,
        &expected,
        state.config.llm.embedding_dim,
        |document, chunk| {
            records
                .iter()
                .find(|record| record.id() == document.id)
                .is_some_and(|record| {
                    record.metadata_matches(chunk, request.source.conversation_date.as_deref())
                })
        },
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let observed: Vec<_> = report
        .records
        .into_iter()
        .map(|record| {
            json!({
                "id":record.id,"chunk":chunk_status(record.chunk),
                "vector":vector_status(record.vector),"document":document_status(record.document),
            })
        })
        .collect();
    let mut response = json!({"schema_version":1,"scope":request.scope,"evidence":EVIDENCE,
        "dimension":state.config.llm.embedding_dim,"ready":report.ready,"records":observed});
    if include_projection {
        response["schema_version"] = json!(2);
        response["evidence"] = json!("fresh_immutable_ingest_projections_v2");
        // Reconstructed content has already passed the sink sanitizer. Never
        // expose a partial projection when the indexed records are unready.
        response["fragment_projections"] = if report.ready {
            json!(records
                .iter()
                .filter_map(|record| match record {
                    Record::Source(source) => source.fragment.as_ref().map(|meta| json!({
                        "id":source.id,"content":source.content,"fragment":meta
                    })),
                    Record::Fact(_, _) => None,
                })
                .collect::<Vec<_>>())
        } else {
            Value::Null
        };
    }
    Ok(Json(response))
}

#[cfg(test)]
mod tests;
