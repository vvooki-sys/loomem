//! Reranker stage of `/v1/search`: the cost gate, the candidate slice, the
//! document text handed to the model and the model call itself. Extracted
//! verbatim from the orchestrator so the rerank levers (date-prefixed
//! documents, synchronous rerank, truncation) can evolve here without
//! touching `search.rs`. By default the stage is async-to-cache only: it
//! spawns the model call and hands the ranked pool back unchanged. With
//! `[search] rerank_sync` on it awaits the model on the request path, within
//! `rerank_sync_timeout_ms`, and returns the reranked order.
use super::{cache_policy, prepare_llm_input, LaneOutcome, QueryContext};
use crate::auth::AuthContext;
use crate::handlers::types::SearchRequest;
use crate::AppState;
use loomem_core::config::{LlmConfig, SearchConfig};
#[cfg(feature = "onnx-rerank")]
use loomem_core::onnx_reranker::OnnxReranker;
use loomem_core::storage::RocksDbStore;
use loomem_core::{reranker, HybridSearchResult};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
mod tests;

/// What the orchestrator hands to [`stage`]: the filtered, ranked pool and
/// the bits the async publish needs to key the cache entry.
pub(super) struct RerankInput<'a> {
    pub ranked: Vec<HybridSearchResult>,
    pub final_top_k: usize,
    /// Rare-term lane outcome (`None` when the lane is off).
    pub lane: Option<&'a LaneOutcome>,
    pub cache_generation: u64,
}

/// Rerank stage. Checks the cost budget, and — reranker on, pool non-empty,
/// request cacheable — spawns the background rerank whose ordering lands in
/// the reranked cache slot for the next identical query. Returns the ranked
/// pool unchanged: the fresh response keeps the fused order.
///
/// With `[search] rerank_sync` on, the same gate leads to [`sync_rerank`]
/// instead: the model runs on the request path (cacheable or not) and the
/// fresh response carries its order, or the fused order on error/timeout.
pub(super) async fn stage(
    state: &Arc<AppState>,
    ctx: &QueryContext,
    payload: &SearchRequest,
    auth: &AuthContext,
    input: RerankInput<'_>,
) -> Vec<HybridSearchResult> {
    // ECA-31: Check cost budget for reranker
    let rerank_cost_ok = rerank_budget_allows(state, auth);

    // Async reranking: spawn in background, results cached for next identical query
    if state.config.search.rerank_enabled && rerank_cost_ok && !input.ranked.is_empty() {
        // W2a: inline instead, behind the same gate.
        if state.config.search.rerank_sync {
            let backend = Backend::from_state(state);
            return sync_rerank(state, payload, input, ctx.cache_key, backend).await;
        }
        if let Some(key) = ctx.cache_key {
            spawn_async(state, payload, &input, key);
        }
    }
    input.ranked
}

/// Synchronous rerank (`rerank_sync`): await the model within
/// `rerank_sync_timeout_ms`. On success the reordered pool (cut to
/// `final_top_k`, membership unchanged) is returned and — cacheable request
/// only — also published to the reranked cache slot, exactly what the async
/// path would publish. On a model error or timeout the fused pool is
/// returned unchanged and no async rerank is started in its place.
///
/// "Error" in practice means the ONNX cross-encoder or its blocking task:
/// `reranker::rerank` maps every LLM provider failure (no key, HTTP error,
/// timeout) to `Ok(identity)`, so a failing LLM reranker yields the fused
/// order with no warning here.
///
/// On timeout the model future is dropped: an LLM request is cancelled, but
/// an ONNX `spawn_blocking` scoring call cannot be and runs on as an orphan
/// until it finishes. It owns only the query, the documents and the
/// `Arc<OnnxReranker>` (see [`Backend`]) — never `AppState` or the store —
/// and its result is discarded.
///
/// The soft-delete filter ran before the wait, so every path re-applies it
/// afterwards ([`still_visible`]): a chunk deleted or purged while the model
/// ran never reaches the fresh response. (The cache needs no such pass: a
/// delete clears the cache and bumps its generation, so the publish below
/// is refused.)
async fn sync_rerank<B: RerankBackend>(
    state: &Arc<AppState>,
    payload: &SearchRequest,
    input: RerankInput<'_>,
    cache_key: Option<u64>,
    backend: B,
) -> Vec<HybridSearchResult> {
    let job = RerankJob::prepare(state, payload, &input);
    let top_k = input.final_top_k;
    let timeout_ms = state.config.search.rerank_sync_timeout_ms;
    let counters = &state.retrieval_counters;
    counters.record_rerank_invocation();
    let call = backend.run_model(job.query, job.documents, top_k);
    let indices = match tokio::time::timeout(Duration::from_millis(timeout_ms), call).await {
        Ok(Ok(indices)) => indices,
        Ok(Err(e)) => {
            tracing::warn!("Sync rerank failed, keeping fused order: {}", e);
            return still_visible(state, input.ranked);
        }
        Err(_) => {
            tracing::warn!(timeout_ms, "Sync rerank timed out, keeping fused order");
            return still_visible(state, input.ranked);
        }
    };
    counters.record_rerank_completion();
    if let Some(key) = cache_key {
        cache_policy::publish_reranked(
            &state.query_cache,
            (key, input.cache_generation),
            indices.clone(),
            &job.candidates,
            &input.ranked,
            top_k,
        )
        .await;
    }
    // The whole pool in model order, so a hit deleted during the wait is
    // replaced by the next fused one before the `final_top_k` cut.
    let pool_len = input.ranked.len();
    let reordered =
        cache_policy::reranked_results(indices, &job.candidates, &input.ranked, pool_len);
    let mut visible = still_visible(state, reordered);
    visible.truncate(top_k);
    visible
}

/// The pre-rerank soft-delete filter again (same predicate: deleted, purged
/// or unreadable chunks go), for results held across the model wait.
fn still_visible(
    state: &Arc<AppState>,
    mut results: Vec<HybridSearchResult>,
) -> Vec<HybridSearchResult> {
    results.retain(|r| !crate::handlers::delete::is_deleted_in_store(state, &r.id));
    results
}

/// One model call's inputs, built on the request path: the candidate slice,
/// the document text scored for each, and the sanitized query.
struct RerankJob {
    candidates: Vec<HybridSearchResult>,
    documents: Vec<String>,
    query: String,
}

impl RerankJob {
    fn prepare(state: &AppState, payload: &SearchRequest, input: &RerankInput<'_>) -> Self {
        let head = input
            .ranked
            .len()
            .min(state.config.search.rerank_candidates);
        // Cycle/012: the reranker slice is the head of the ranked pool
        // plus — when the rare-term lane is active — any guaranteed ids
        // that survived the pool cut but fell below the head. Lane off
        // (`lane == None`) → exactly the pre-cycle `[..candidates]`.
        let candidates = rerank_slice_with_lane(
            &input.ranked,
            head,
            input.lane,
            state.config.search.rare_term_lane.candidate_cap,
        );
        // Built here, not in a spawned task: a detached task must not hold
        // the store (hosted scopes close and reopen their database while a
        // rerank can still be scoring). With `rerank_date_prefix` on this is
        // one chunk read per candidate (`rerank_candidates`, default 10).
        let documents = documents(&candidates, &state.store, &state.config.search);
        let query = prepare_llm_input(&payload.query, "reranker::rerank");
        Self {
            candidates,
            documents,
            query,
        }
    }
}

/// The text the reranker scores for each candidate. Default: the raw
/// content, without touching the store. With `rerank_date_prefix` on, the
/// content is prefixed with the same date `memory_search` lines show
/// (`[event:…]` / `[observed:…]` / `[recorded:…]` for ingest provenance,
/// else the extracted event date, else the hit's own timestamp) — the
/// prefix counts toward the LLM reranker's `rerank_llm_max_chars`.
pub(super) fn documents(
    candidates: &[HybridSearchResult],
    store: &RocksDbStore,
    cfg: &SearchConfig,
) -> Vec<String> {
    if !cfg.rerank_date_prefix {
        return candidates.iter().map(|r| r.content.clone()).collect();
    }
    candidates
        .iter()
        .map(|r| dated_document(store, r))
        .collect()
}

/// `"[{date}] {content}"`; the bare content when no date can be derived.
fn dated_document(store: &RocksDbStore, r: &HybridSearchResult) -> String {
    let chunk = store.get_chunk(&r.id).ok().flatten();
    let ts = crate::handlers::ingest_date::date_prefix(chunk.as_ref(), Some(r.timestamp));
    if ts.is_empty() {
        r.content.clone()
    } else {
        format!("[{ts}] {}", r.content)
    }
}

fn spawn_async(state: &Arc<AppState>, payload: &SearchRequest, input: &RerankInput<'_>, key: u64) {
    let top_k = input.final_top_k;
    let RerankJob {
        candidates: results_for_rerank,
        documents: chunks,
        query: query_for_rerank,
    } = RerankJob::prepare(state, payload, input);
    let eligible_results = input.ranked.clone();
    let cache = state.query_cache.clone();
    let backend = Backend::from_state(state);
    let cache_generation = input.cache_generation;
    let counters = Arc::clone(&state.retrieval_counters);
    counters.record_rerank_invocation();

    tokio::spawn(async move {
        match backend.run_model(query_for_rerank, chunks, top_k).await {
            Ok(indices) => {
                counters.record_rerank_completion();
                cache_policy::publish_reranked(
                    &cache,
                    (key, cache_generation),
                    indices,
                    &results_for_rerank,
                    &eligible_results,
                    top_k,
                )
                .await;
            }
            Err(e) => {
                tracing::warn!("Async rerank failed (non-blocking): {}", e);
            }
        }
    });
}

/// The model behind the stage: the ONNX cross-encoder when the feature is
/// compiled in and a model loaded, else the LLM reranker.
struct Backend {
    http_client: reqwest::Client,
    llm: LlmConfig,
    /// `[search] rerank_llm_max_chars` — per-document cap for the LLM path.
    llm_max_chars: usize,
    #[cfg(feature = "onnx-rerank")]
    onnx: Option<Arc<OnnxReranker>>,
}

impl Backend {
    fn from_state(state: &AppState) -> Self {
        Self {
            http_client: state.http_client.clone(),
            llm: state.config.llm.clone(),
            llm_max_chars: state.config.search.rerank_llm_max_chars,
            #[cfg(feature = "onnx-rerank")]
            onnx: state.onnx_reranker.as_ref().map(Arc::clone),
        }
    }
}

/// The model call as a seam: [`Backend`] in production, stubs in tests
/// (a fixed order, a failure, a call that never returns).
trait RerankBackend {
    /// Score `documents` against `query`; returns the reordered indices
    /// (best first) of the top `top_k`.
    fn run_model(
        self,
        query: String,
        documents: Vec<String>,
        top_k: usize,
    ) -> impl Future<Output = anyhow::Result<Vec<usize>>> + Send;
}

impl RerankBackend for Backend {
    async fn run_model(
        self,
        query: String,
        documents: Vec<String>,
        top_k: usize,
    ) -> anyhow::Result<Vec<usize>> {
        #[cfg(feature = "onnx-rerank")]
        if let Some(onnx) = self.onnx {
            return match tokio::task::spawn_blocking(move || onnx.rerank(&query, &documents, top_k))
                .await
            {
                Ok(result) => result,
                Err(error) => Err(anyhow::anyhow!("ONNX rerank task failed: {error}")),
            };
        }
        reranker::rerank(
            &self.http_client,
            &self.llm,
            &query,
            &documents,
            top_k,
            self.llm_max_chars,
        )
        .await
    }
}

/// Reranker slice: the ranked head (`[..head]`, pre-cycle behaviour) plus —
/// lane only — guaranteed ids that survived the pool cut but sit below the
/// head, capped at `cap` extras. Lane off → exactly the pre-cycle slice.
fn rerank_slice_with_lane(
    ranked: &[HybridSearchResult],
    head: usize,
    lane: Option<&LaneOutcome>,
    cap: usize,
) -> Vec<HybridSearchResult> {
    let mut slice: Vec<_> = ranked[..head].to_vec();
    let Some(lane) = lane else {
        return slice;
    };
    let mut appended = 0usize;
    for r in ranked.iter().skip(head) {
        if appended >= cap {
            break;
        }
        if lane.ids.contains(&r.id) {
            slice.push(r.clone());
            appended += 1;
        }
    }
    slice
}

fn rerank_budget_allows(state: &Arc<AppState>, auth: &AuthContext) -> bool {
    let cost_status = loomem_core::cost_tracker::check_cost_budget_for_stream(
        &state.store,
        &auth.stream_id,
        state.config.cost.daily_cap_usd,
    );
    if !cost_status.allow_reranker() {
        tracing::warn!(
            "Reranker disabled for stream {}: cost budget tier={} (ECA-31)",
            auth.stream_id,
            cost_status.description()
        );
    }
    cost_status.allow_reranker()
}
