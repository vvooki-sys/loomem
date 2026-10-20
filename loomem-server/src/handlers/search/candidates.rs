//! Request-aware candidate budgets and the primary hybrid retrieval branch.
use std::collections::HashSet;
use std::sync::Arc;

use loomem_core::hybrid_search::{SearchCandidateBudget, VectorSearchOptions};
use loomem_core::{embeddings, HybridSearchResult, SearchResult};
use tracing::warn;

use super::{
    retain_by_agent_set, vector_channel_hits, QueryComplexity, QueryContext, RequestTelemetry,
};
use crate::handlers::types::{ChannelDiagnostics, SearchRequest};
use crate::handlers::AppError;
use crate::AppState;

pub(super) struct PrimaryRequest<'a> {
    pub ctx: &'a QueryContext,
    pub payload: &'a SearchRequest,
    pub guaranteed: Option<&'a HashSet<String>>,
    pub channel_diag: Option<&'a mut ChannelDiagnostics>,
    pub telemetry: Option<&'a RequestTelemetry>,
}

struct VectorInputs {
    embeddings: Vec<(String, Vec<f32>)>,
    query_embedding: Vec<f32>,
}

/// Preserve the existing final-limit/default policy while validating before any
/// retrieval multiplier/allocation. MCP request caps remain independent.
pub(super) fn request_limit(
    state: &AppState,
    payload: &SearchRequest,
    complexity: QueryComplexity,
) -> Result<usize, AppError> {
    if payload
        .top_k
        .is_some_and(|k| k == 0 || k > state.config.search.max_request_top_k)
    {
        return Err(AppError::BadRequest(format!(
            "top_k must be between 1 and {}",
            state.config.search.max_request_top_k
        )));
    }
    let limit = payload.top_k.unwrap_or(state.config.search.top_k);
    candidate_budget(state, limit)?;
    let classified_limit = classified_limit(state, payload, complexity);
    tracing::debug!(complexity = %complexity, top_k = classified_limit, "Query classified");
    Ok(limit)
}

fn classified_limit(
    state: &AppState,
    payload: &SearchRequest,
    complexity: QueryComplexity,
) -> usize {
    let default = if state.config.search.complexity.enabled {
        match complexity {
            QueryComplexity::Simple => state.config.search.complexity.simple_top_k,
            QueryComplexity::Medium => state.config.search.complexity.medium_top_k,
            QueryComplexity::Complex | QueryComplexity::Temporal => {
                state.config.search.complexity.complex_top_k
            }
            QueryComplexity::Aggregation => 30,
            QueryComplexity::Profile => state.config.search.top_k,
        }
    } else {
        state.config.search.top_k
    };
    payload.top_k.unwrap_or(default)
}

fn candidate_budget(state: &AppState, limit: usize) -> Result<SearchCandidateBudget, AppError> {
    SearchCandidateBudget::new(state.config.search.top_k, limit)
        .map_err(|_| AppError::BadRequest("top_k candidate budget overflow".into()))
}

pub(super) async fn retrieve_primary(
    state: &Arc<AppState>,
    bm25_results: Vec<SearchResult>,
    agent_set: Option<&HashSet<String>>,
    request: PrimaryRequest<'_>,
) -> Result<Vec<HybridSearchResult>, AppError> {
    let budget = candidate_budget(state, request.ctx.limit)?;
    let vectors = if state.config.storage.vector_enabled
        && request.ctx.complexity != QueryComplexity::Simple
    {
        vector_inputs(
            state,
            request.ctx,
            request.payload,
            agent_set,
            request.telemetry,
        )
        .await
    } else {
        None
    };
    let Some(inputs) = vectors else {
        return Ok(state.hybrid_search.bm25_only_with_budget(
            bm25_results,
            request.guaranteed,
            budget,
        )?);
    };
    if let Some(diag) = request.channel_diag {
        diag.vector_top = vector_channel_hits(&inputs.embeddings, &inputs.query_embedding);
    }
    Ok(state.hybrid_search.search_with_vector_budget(
        bm25_results,
        &inputs.embeddings,
        &inputs.query_embedding,
        VectorSearchOptions {
            store: Some(&state.store),
            stream_filter: None, // Already stream-filtered before vector ranking.
            guaranteed: request.guaranteed,
            budget,
        },
    )?)
}

async fn vector_inputs(
    state: &AppState,
    ctx: &QueryContext,
    payload: &SearchRequest,
    agent_set: Option<&HashSet<String>>,
    telemetry: Option<&RequestTelemetry>,
) -> Option<VectorInputs> {
    if let Some(meter) = telemetry {
        meter.record_embedding_attempt();
    }
    let query_embedding = match embed_query(state, &payload.query).await {
        Ok(embedding) => embedding,
        Err(e) => {
            warn!("Failed to embed query: {}, falling back to BM25", e);
            return None;
        }
    };
    let all = match state.store.get_all_embeddings() {
        Ok(all) => retain_by_agent_set(all, agent_set),
        Err(e) => {
            warn!("Failed to retrieve embeddings: {}, falling back to BM25", e);
            return None;
        }
    };
    if all.is_empty() {
        tracing::info!("No embeddings found, falling back to BM25 only");
        return None;
    }
    let all = super::event_date::embeddings(all, ctx.date_filter.as_ref(), state);
    let filtered = filter_stream_embeddings(state, all, ctx.stream_list.as_deref());
    tracing::info!(
        "Performing hybrid search with {} embeddings (stream-filtered)",
        filtered.len()
    );
    if filtered.is_empty() {
        None
    } else {
        Some(VectorInputs {
            embeddings: filtered,
            query_embedding,
        })
    }
}

fn filter_stream_embeddings(
    state: &AppState,
    embeddings: Vec<(String, Vec<f32>)>,
    streams: Option<&[String]>,
) -> Vec<(String, Vec<f32>)> {
    let Some(streams) = streams else {
        return embeddings;
    };
    embeddings
        .into_iter()
        .filter(|(id, _)| {
            state
                .store
                .get_chunk(id)
                .ok()
                .flatten()
                .is_some_and(|chunk| streams.contains(&chunk.stream))
        })
        .collect()
}

async fn embed_query(state: &AppState, query: &str) -> anyhow::Result<Vec<f32>> {
    if let Some(embedder) = &state.local_embedder {
        return embedder.embed(query);
    }
    let api_key = state
        .config
        .llm
        .get_api_key()
        .ok_or_else(|| anyhow::anyhow!("No embedding provider available"))?;
    embeddings::embed(
        &state.http_client,
        &api_key,
        &state.config.llm.embedding_model,
        query,
    )
    .await
}
