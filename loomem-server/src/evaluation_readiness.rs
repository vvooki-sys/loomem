//! Private observation protocol for fresh, immutable, harness-owned evaluations.
//! Authentication/opt-in do not establish freshness or exclude external writers.
use std::{collections::HashSet, sync::Arc};

use axum::{
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::post,
    Extension, Json, Router,
};
use loomem_core::evaluation_readiness::{
    probe_expected_records_with_metadata, ChunkReadiness, VectorReadiness,
};
use loomem_core::tantivy_index::readiness::{DocumentReadiness, ExpectedDocument};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::{self, AuthConfig, AuthContext},
    mcp::dispatcher::expected_bytes::{self, SourceExpectation},
    AppState,
};

const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_RECORDS: usize = 1024;
const EVIDENCE: &str = "fresh_immutable_source_fragments_v1";

/// Mounted only when the build carries the `evaluation` feature, the operator
/// opted in with `LOOMEM_EVALUATION_READINESS=1` at startup, and a non-empty
/// admin token exists; otherwise the router is empty and the paths answer 404.
/// A stock `config.toml` still answers 503 (see [`compatible`]) — intended.
pub(crate) fn routes(auth: &AuthConfig) -> Router<Arc<AppState>> {
    let enabled = std::env::var("LOOMEM_EVALUATION_READINESS").as_deref() == Ok("1");
    if mounted(auth, enabled) {
        tracing::info!(
            "Evaluation mode: readiness routes mounted (/v1/evaluation/readiness, \
             /v1/evaluation/ingest-readiness, /v1/evaluation/ingest-readiness-v2); \
             LOOMEM_EVALUATION_READINESS=1 and an admin token are set"
        );
    }
    routes_enabled(auth, enabled)
}

fn mounted(auth: &AuthConfig, enabled: bool) -> bool {
    enabled
        && auth
            .admin_token
            .as_ref()
            .is_some_and(|token| !token.is_empty())
}

fn routes_enabled(auth: &AuthConfig, enabled: bool) -> Router<Arc<AppState>> {
    if !mounted(auth, enabled) {
        return Router::new();
    }
    Router::new()
        .route("/v1/evaluation/readiness", post(observe))
        .route("/v1/evaluation/ingest-readiness", post(ingest::observe))
        .route(
            "/v1/evaluation/ingest-readiness-v2",
            post(ingest::observe_v2),
        )
        .layer(DefaultBodyLimit::max(MAX_BYTES))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema_version: u32,
    scope: String,
    stream: String,
    sources: Vec<SourceExpectation>,
}

fn compatible(config: &loomem_core::Config) -> bool {
    let loopback = config
        .server
        .host
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback());
    let enabled_mutators = [
        config.scheduler.enabled,
        config.dream.enabled,
        config.knowledge_extraction.enabled,
        config.entity_extraction.enabled,
        config.memory_generator.enabled,
        config.contradiction.enabled,
        config.profile.enabled,
        config.content_type.enabled,
        config.associator.enabled,
    ];
    loopback
        && !enabled_mutators.into_iter().any(|enabled| enabled)
        && config.llm.embedding_provider == "local"
        && config.llm.embedding_dim == 384
        && config.storage.tantivy.enabled
        && config.storage.vector_enabled
        && config.fragments.enabled
}

/// Shared envelope check for every evaluation request body.
fn valid_header(schema_version: u32, scope: &str) -> bool {
    schema_version == 1
        && !scope.is_empty()
        && scope.len() <= 128
        && !scope.chars().any(char::is_control)
}

fn validate(request: &Request) -> Result<(), StatusCode> {
    if !valid_header(request.schema_version, &request.scope) || request.sources.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let mut ids = HashSet::new();
    for source in &request.sources {
        for id in std::iter::once(&source.source_id).chain(&source.fragment_ids) {
            if id.is_empty() || id.len() > 256 || !ids.insert(id) || ids.len() > MAX_RECORDS {
                return Err(StatusCode::BAD_REQUEST);
            }
        }
    }
    Ok(())
}

async fn observe(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Json(request): Json<Request>,
) -> Result<Json<Value>, StatusCode> {
    validate(&request)?;
    let stream = auth::validate_stream(&auth, Some(&request.stream))?;
    if !compatible(&state.config) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let mut records = Vec::new();
    for source in &request.sources {
        let reconstructed =
            expected_bytes::reconstruct(&state, source).map_err(|_| StatusCode::BAD_REQUEST)?;
        records.extend(
            reconstructed
                .into_iter()
                .map(|record| (record, source.conversation_date.as_deref())),
        );
    }
    let expected: Vec<_> = records
        .iter()
        .map(|(record, _)| ExpectedDocument {
            id: &record.id,
            content: &record.content,
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
                .find(|(record, _)| record.id == document.id)
                .is_some_and(|(record, date)| {
                    expected_bytes::metadata_matches(record, chunk, *date)
                })
        },
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let observed: Vec<_> = report.records.into_iter().map(|record| json!({
        "id": record.id, "chunk": chunk_status(record.chunk),
        "vector": vector_status(record.vector), "document": document_status(record.document),
    })).collect();
    Ok(Json(
        json!({"schema_version":1, "scope":request.scope, "evidence":EVIDENCE,
        "dimension":state.config.llm.embedding_dim, "ready":report.ready, "records":observed}),
    ))
}

fn chunk_status(status: ChunkReadiness) -> &'static str {
    match status {
        ChunkReadiness::Active => "active",
        ChunkReadiness::Unavailable => "unavailable",
        ChunkReadiness::ContentMismatch => "content_mismatch",
        ChunkReadiness::MetadataMismatch => "metadata_mismatch",
    }
}
fn vector_status(status: VectorReadiness) -> &'static str {
    match status {
        VectorReadiness::Valid => "valid",
        VectorReadiness::Missing => "missing",
        VectorReadiness::Invalid => "invalid",
        VectorReadiness::Unavailable => "unavailable",
    }
}
fn document_status(status: DocumentReadiness) -> &'static str {
    match status {
        DocumentReadiness::Current => "current",
        DocumentReadiness::Missing => "missing",
        DocumentReadiness::Duplicate => "duplicate",
        DocumentReadiness::ContentMismatch => "content_mismatch",
    }
}

#[cfg(test)]
mod tests;

mod ingest;
