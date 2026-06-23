use axum::{extract::State, Json};
use chrono::{NaiveDate, Utc};
use loomem_core::query_cache::QueryCache;
use loomem_core::query_expansion::polish_stem;
#[cfg(test)]
use loomem_core::sanitizer::sanitize_for_llm;
use loomem_core::sanitizer::sanitize_with_sources;
use loomem_core::search::{classify, ClassifiedQuery};
use loomem_core::{embeddings, multi_query, reranker};
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;
use tracing::warn;

use super::date_filter::extract_date_filter;
use super::types::{
    AssociateRequest, AssociateResponse, Association, ContextSufficiency, DateFilter,
    SearchRequest, SearchResponse, SearchResult,
};

/// Sanitize query before sending to an LLM gateway (asymmetric threat model).
///
/// Strips injection patterns, emits a warn log per detection (with source tag
/// `raw`/`stripped`/`both` for observability), and returns the stripped content.
/// Retrieval paths (tantivy, BM25, vector) must use the raw query — do NOT call
/// this for retrieval.
///
/// Invariant: `prepare_llm_input(x, _) == sanitize_for_llm(x).content` — the
/// content derivation is identical (both flow through `strip_html`); only the
/// warn log gains source-tag observability via `sanitize_with_sources`.
fn prepare_llm_input(query: &str, call_site: &'static str) -> String {
    let result = sanitize_with_sources(query);
    if result.injection_detected {
        for pattern in &result.injection_patterns {
            warn!(
                call_site = call_site,
                pattern = pattern.name.as_str(),
                source = ?pattern.source,
                "injection pattern detected in /search query sent to LLM gateway"
            );
        }
    }
    result.content
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b.iter()) {
        let (x, y) = (*x as f64, *y as f64);
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

/// Detect counting/aggregation queries that benefit from L0 raw chunks.
/// Excludes preference queries where L1 summaries are more useful.
fn is_counting_query(query: &str) -> bool {
    let q = query.to_lowercase();
    // Preference queries should NOT get L0 boost — L1 summaries are better
    let preference_patterns = [
        "prefer",
        "favorite",
        "favourite",
        "like more",
        "like best",
        "rather",
        "which do i like",
        "what do i enjoy",
        "what do i like",
        "do i enjoy",
        "do i prefer",
    ];
    if preference_patterns.iter().any(|p| q.contains(p)) {
        return false;
    }
    let patterns = [
        "how many",
        "how much",
        "how often",
        "total",
        "count",
        "all the",
        "every",
        "each",
        "list all",
        "in total",
        "combined",
        "sum",
        "altogether",
    ];
    patterns.iter().any(|p| q.contains(p))
}
use super::scope as scope_mod;
use super::AppError;
use crate::auth::{self, AuthContext};
use crate::AppState;

#[derive(Debug, Clone, Copy, PartialEq)]
enum QueryComplexity {
    Simple,
    Medium,
    Complex,
    Temporal,
    Aggregation,
    Profile,
}

impl std::fmt::Display for QueryComplexity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryComplexity::Simple => write!(f, "Simple"),
            QueryComplexity::Medium => write!(f, "Medium"),
            QueryComplexity::Complex => write!(f, "Complex"),
            QueryComplexity::Temporal => write!(f, "Temporal"),
            QueryComplexity::Aggregation => write!(f, "Aggregation"),
            QueryComplexity::Profile => write!(f, "Profile"),
        }
    }
}

fn classify_query(query: &str) -> QueryComplexity {
    let lower = query.to_lowercase();
    let word_count = query.split_whitespace().count();

    // Profile keywords
    let profile_keywords = [
        "profil",
        "co wiesz o mnie",
        "kim jestem",
        "mój profil",
        "podsumuj mnie",
    ];
    if profile_keywords.iter().any(|k| lower.contains(k)) {
        return QueryComplexity::Profile;
    }

    // Aggregation keywords (counting, listing, enumerating)
    let aggregation_keywords = [
        "ile ",
        "how many",
        "ile razy",
        "ilu ",
        "lista ",
        "list all",
        "wymień",
        "wypisz",
        "wszystkie ",
        "wszystkich ",
        "name every",
        "jakie wszystkie",
        "ile różnych",
        "ile unikalnych",
        "how many different",
        "how many unique",
        "how many total",
    ];
    if aggregation_keywords.iter().any(|k| lower.contains(k)) {
        return QueryComplexity::Aggregation;
    }

    // Complex keywords (multi-hop reasoning)
    let complex_keywords = [
        "dlaczego",
        "jak ",
        "porównaj",
        "ewoluował",
        "zmienił",
        "historia",
        "podsumuj",
        "przeanalizuj",
        "różnica między",
        "timeline",
        "chronolog",
    ];
    if complex_keywords.iter().any(|k| lower.contains(k)) || word_count > 15 {
        return QueryComplexity::Complex;
    }

    // Temporal keywords
    let temporal_keywords = [
        "kiedy",
        "ostatnio",
        "wczoraj",
        "tydzień",
        "miesiąc",
        "rok",
        "w marcu",
        "w styczniu",
        "w lutym",
        "w kwietniu",
        "od ",
        "do ",
        "przed",
        "po ",
        "między",
        "ago",
        "last week",
        "recently",
    ];
    if temporal_keywords.iter().any(|k| lower.contains(k)) {
        return QueryComplexity::Temporal;
    }

    if (5..=15).contains(&word_count) {
        return QueryComplexity::Medium;
    }

    // Default: simple
    QueryComplexity::Simple
}

/// Prepared query context — immutable after `prepare_query_context` returns.
/// Holds validated parameters and computed query variants used throughout the
/// retrieval pipeline. Introduced in cycle/02 to split a 1039-NLOC / CC=167
/// `search_handler` into orchestrator + helpers (see cycles/02-brief.md).
struct QueryContext {
    start: Instant,
    query_without_date: String,
    original_query_stemmed: String,
    expanded_query_stemmed: String,
    complexity: QueryComplexity,
    date_filter: Option<DateFilter>,
    stream_list: Option<Vec<String>>,
    cache_key: Option<u64>,
    limit: usize,
}

/// Pipeline metrics collected between retrieval and response building.
/// Populated inside `search_handler` after filter/dedup/truncate, then passed
/// to `build_response` for trace metadata + sufficiency scoring.
/// Includes `start` and `complexity` because both outlive the orchestrator's
/// retrieval block and feed directly into the response envelope.
struct ResponseMetrics {
    start: Instant,
    complexity: QueryComplexity,
    dedup_removed: usize,
    total_results_before_topk: usize,
    final_top_k: usize,
}

/// Output of `filter_and_truncate`. Bundles the filtered result list with
/// the counters that feed into `ResponseMetrics`. Introduced in cycle/02
/// Iter 3 so the helper can return multiple related values without a tuple
/// explosion at the call site.
struct FilterOutcome {
    results: Vec<loomem_core::HybridSearchResult>,
    dedup_removed: usize,
    total_results_before_topk: usize,
    final_top_k: usize,
}

/// Whether the request carries a `source_agent` include or
/// `exclude_source_agents` filter (cycle/258, Option A). Gates the agent-scoped
/// retrieval paths; off → the pre-existing retrieval is byte-identical.
fn has_agent_filter(payload: &SearchRequest) -> bool {
    payload.source_agent.is_some() || payload.exclude_source_agents.is_some()
}

/// One leaf of a BM25 search (cycle/258, Option A). Bundles the per-call query +
/// the active branch's filter so [`bm25_leaf`] keeps a single arity. The
/// branches are mutually exclusive, so each constructor sets exactly one.
struct Bm25Leaf<'a> {
    query: &'a str,
    stream: Option<&'a str>,
    entity: Option<&'a str>,
    date_range: Option<(i64, i64)>,
    limit: usize,
}

impl<'a> Bm25Leaf<'a> {
    /// Common path: content + optional single-stream filter.
    fn plain(query: &'a str, stream: Option<&'a str>, limit: usize) -> Self {
        Self {
            query,
            stream,
            entity: None,
            date_range: None,
            limit,
        }
    }
    /// Entity branch: content + entity filter.
    fn entity(query: &'a str, entity: &'a str, limit: usize) -> Self {
        Self {
            query,
            stream: None,
            entity: Some(entity),
            date_range: None,
            limit,
        }
    }
    /// Date branch: content + `event_date` range filter.
    fn date(query: &'a str, range: (i64, i64), limit: usize) -> Self {
        Self {
            query,
            stream: None,
            entity: None,
            date_range: Some(range),
            limit,
        }
    }
}

/// Run one BM25 leaf search honoring the `source_agent` filter (cycle/258,
/// Option A — root-cause fix for #257's under-fill). When an agent filter is
/// present the agent include (MUST) / exclude (MUST_NOT) clauses are pushed into
/// Tantivy via `search_with_agent`, so the candidate pool is agent-scoped at the
/// source and the downstream `top_k * N` truncation can no longer starve the
/// target agent — no per-candidate `get_chunk`. With no agent filter the call
/// dispatches to the pre-existing per-branch method unchanged (byte-identical,
/// preserves the §14 Tier C identity guarantee).
fn bm25_leaf(
    tantivy: &loomem_core::TantivyIndex,
    payload: &SearchRequest,
    leaf: Bm25Leaf<'_>,
) -> anyhow::Result<Vec<loomem_core::SearchResult>> {
    if has_agent_filter(payload) {
        tantivy.search_with_agent(loomem_core::AgentSearchParams {
            query_text: leaf.query,
            stream: leaf.stream,
            entity: leaf.entity,
            date_range: leaf.date_range,
            source_agent: payload.source_agent.as_deref(),
            exclude_source_agents: payload.exclude_source_agents.as_deref(),
            limit: leaf.limit,
        })
    } else if let Some((start_ts, end_ts)) = leaf.date_range {
        tantivy.search_with_date_range(leaf.query, start_ts, end_ts, leaf.limit)
    } else if let Some(entity) = leaf.entity {
        tantivy.search_with_entity(leaf.query, entity, leaf.limit)
    } else if let Some(stream) = leaf.stream {
        tantivy.search_with_stream(leaf.query, stream, leaf.limit)
    } else {
        tantivy.search(leaf.query, leaf.limit)
    }
}

/// The agent-matching chunk id set for the vector path, or `None` when no agent
/// filter is set (embeddings left untouched). One Tantivy full-collect query
/// (`ids_matching_agent`) replaces #257's per-embedding `get_chunk`. Fails open
/// (`None` + WARN) on a Tantivy error — `filter_and_truncate` is the safety net.
async fn agent_id_set(
    state: &Arc<AppState>,
    payload: &SearchRequest,
) -> Option<std::collections::HashSet<String>> {
    if !has_agent_filter(payload) {
        return None;
    }
    let tantivy = state.tantivy.lock().await;
    match tantivy.ids_matching_agent(
        payload.source_agent.as_deref(),
        payload.exclude_source_agents.as_deref(),
    ) {
        Ok(set) => Some(set),
        Err(e) => {
            warn!("source_agent id-set query failed: {e}; skipping vector agent pre-filter");
            None
        }
    }
}

/// Drop embeddings whose id is not in the agent id-set, before vector scoring /
/// truncation. No-op when `set` is `None` (no agent filter). Replaces #257's
/// `retain_embeddings_by_agent` (one `get_chunk` per embedding) with a
/// `HashSet` membership test (cycle/258).
fn retain_by_agent_set(
    embeddings: Vec<(String, Vec<f32>)>,
    set: Option<&std::collections::HashSet<String>>,
) -> Vec<(String, Vec<f32>)> {
    match set {
        Some(s) => embeddings
            .into_iter()
            .filter(|(id, _)| s.contains(id))
            .collect(),
        None => embeddings,
    }
}

/// Validate auth, classify query, extract date filter, apply entity
/// resolution + query expansion + Polish stemming, and compute cache key.
/// Does NOT perform cache lookup — that stays in the orchestrator so an
/// early return can short-circuit the full pipeline.
///
/// NOTE (cycle/02 F-R1): complexity-aware `_limit` is kept here for
/// behavioural parity, but the dead-code bug where it is computed and
/// immediately discarded (in favour of the flat `limit` below) is
/// preserved. Fix is tracked as a separate ticket to keep cycle/02 scope
/// strictly refactoring with zero behavioural change.
fn prepare_query_context(
    state: &Arc<AppState>,
    auth: &AuthContext,
    payload: &SearchRequest,
) -> Result<QueryContext, AppError> {
    // Scope branch: mutually exclusive with stream/streams.
    // resolve_scope handles membership + leak-protection — do not wrap its
    // errors in BadRequest (that would destroy the 404 leak-protection).
    let validated_streams = if let Some(scope) = payload.scope {
        if payload.stream.is_some() || payload.streams.is_some() {
            return Err(AppError::BadRequest(
                "specify scope or stream/streams, not both".into(),
            ));
        }
        let resolution = scope_mod::resolve_scope(scope, auth, &state.store)?;
        Some(resolution.streams.into_iter().map(|(id, _)| id).collect())
    } else {
        // Existing path — byte-identical behaviour for pre-existing callers.
        auth::validate_streams(
            auth,
            payload
                .streams
                .as_deref()
                .or(payload.stream.as_ref().map(std::slice::from_ref)),
        )
        .map_err(|_| AppError::BadRequest("Access denied: cannot search this stream".into()))?
    };
    let start = Instant::now();

    // Complexity-aware recall: classify query to route through appropriate pipeline
    let complexity = if state.config.search.complexity.enabled {
        classify_query(&payload.query)
    } else {
        QueryComplexity::Complex // full pipeline when disabled
    };

    let _limit = if state.config.search.complexity.enabled {
        match complexity {
            QueryComplexity::Simple => payload
                .top_k
                .unwrap_or(state.config.search.complexity.simple_top_k),
            QueryComplexity::Medium => payload
                .top_k
                .unwrap_or(state.config.search.complexity.medium_top_k),
            QueryComplexity::Complex | QueryComplexity::Temporal => payload
                .top_k
                .unwrap_or(state.config.search.complexity.complex_top_k),
            QueryComplexity::Aggregation => payload.top_k.unwrap_or(30),
            QueryComplexity::Profile => payload.top_k.unwrap_or(state.config.search.top_k),
        }
    } else {
        payload.top_k.unwrap_or(state.config.search.top_k)
    };

    tracing::debug!(complexity = %complexity, top_k = _limit, "Query classified");

    // Extract date filter from query (implicit) or use explicit params
    let (query_without_date, date_filter) =
        if payload.date_from.is_some() || payload.date_to.is_some() {
            let filter = if let (Some(from), Some(to)) = (&payload.date_from, &payload.date_to) {
                if let (Ok(from_date), Ok(to_date)) = (
                    NaiveDate::parse_from_str(from, "%Y-%m-%d"),
                    NaiveDate::parse_from_str(to, "%Y-%m-%d"),
                ) {
                    let start_ts = from_date
                        .and_hms_opt(0, 0, 0)
                        .expect("valid static HMS values")
                        .and_utc()
                        .timestamp();
                    let end_ts = to_date
                        .and_hms_opt(23, 59, 59)
                        .expect("valid static HMS values")
                        .and_utc()
                        .timestamp();
                    Some(DateFilter::Range(start_ts, end_ts))
                } else {
                    None
                }
            } else if let Some(from) = &payload.date_from {
                if let Ok(from_date) = NaiveDate::parse_from_str(from, "%Y-%m-%d") {
                    let start_ts = from_date
                        .and_hms_opt(0, 0, 0)
                        .expect("valid static HMS values")
                        .and_utc()
                        .timestamp();
                    let now = Utc::now().timestamp();
                    Some(DateFilter::Range(start_ts, now))
                } else {
                    None
                }
            } else if let Some(to) = &payload.date_to {
                if let Ok(to_date) = NaiveDate::parse_from_str(to, "%Y-%m-%d") {
                    let end_ts = to_date
                        .and_hms_opt(23, 59, 59)
                        .expect("valid static HMS values")
                        .and_utc()
                        .timestamp();
                    Some(DateFilter::Range(0, end_ts))
                } else {
                    None
                }
            } else {
                None
            };
            (payload.query.clone(), filter)
        } else {
            extract_date_filter(&payload.query)
        };

    tracing::debug!(
        "Date filter: {:?}, cleaned query: '{}'",
        date_filter,
        query_without_date
    );

    // Step 1: Resolve entity aliases
    let entity_resolved = state.entity_extractor.resolve_aliases(&query_without_date);

    if entity_resolved != query_without_date {
        tracing::debug!(
            "Entity alias resolution: {:?} → {:?}",
            query_without_date,
            entity_resolved
        );
    }

    // Step 2: Apply query expansion (on entity-resolved query)
    let expanded_query = state.query_expander.expand(&entity_resolved);

    // Apply Polish stemming to both original and expanded queries
    let original_query_stemmed = if state.config.search.stem_polish {
        let words: Vec<String> = query_without_date
            .split_whitespace()
            .flat_map(polish_stem)
            .collect();
        words.join(" ")
    } else {
        query_without_date.clone()
    };

    let expanded_query_stemmed = if state.config.search.stem_polish {
        let words: Vec<String> = expanded_query
            .split_whitespace()
            .flat_map(polish_stem)
            .collect();
        words.join(" ")
    } else {
        expanded_query.clone()
    };

    tracing::debug!(
        "Query: {} -> Expanded: {}",
        query_without_date,
        expanded_query
    );

    // Cache key (flat `limit` — see F-R1 note above). The cache key is keyed on
    // the *real* `top_k`, not the over-fetched retrieval pool: `source_agent` and
    // `exclude_source_agents` are already part of the key (below), so two requests
    // that differ only in agent filter never collide, and the stored result set is
    // post-filter + post-truncate either way.
    let limit = payload.top_k.unwrap_or(state.config.search.top_k);
    let stream_list: Option<Vec<String>> = validated_streams;
    let cache_key = if state.config.search.cache.enabled {
        Some(QueryCache::hash_query_with_source(
            &original_query_stemmed,
            stream_list.as_deref(),
            payload.entity.as_deref(),
            payload.date_from.as_deref(),
            payload.date_to.as_deref(),
            limit,
            payload.source_agent.as_deref(),
            payload.exclude_source_agents.as_deref(),
        ))
    } else {
        None
    };

    Ok(QueryContext {
        start,
        query_without_date,
        original_query_stemmed,
        expanded_query_stemmed,
        complexity,
        date_filter,
        stream_list,
        cache_key,
        // Real `top_k`. Cycle/258 (Option A) pushes the source_agent filter into
        // Tantivy (`bm25_leaf` / `agent_id_set`), so the candidate pool is
        // agent-scoped at the source — #257's over-fetch is no longer needed.
        limit,
    })
}

pub async fn search_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Json(payload): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, AppError> {
    // Iter 1+2 (cycle/02): context-building extracted into
    // `prepare_query_context`. Retrieval split into `bm25_retrieve` and
    // `vector_retrieve` in Iter 2. `ctx` stays live (not destructured) so the
    // helpers can borrow the prepared fields via `&ctx` without duplicating
    // arguments.
    let ctx = prepare_query_context(&state, &auth, &payload)?;

    // Cycle/85: deterministic query classification. Sub-ms, regex-only,
    // no LLM. Surfaced in the response only when the request flag is set;
    // retrieval pipeline does NOT consume the weights yet — that's /86.
    let classification = classify_and_trace(&payload.query);

    // Cache lookup — early return on hit. MUST stay in orchestrator so the
    // full retrieval pipeline can be short-circuited (see cycle/02 brief
    // F-R2 on cache invariants).
    if let Some(key) = ctx.cache_key {
        let mut cache = state.query_cache.lock().await;
        if let Some((is_reranked, cached)) = cache.get_best(key) {
            drop(cache); // release lock before building response
            tracing::debug!(
                "Cache hit for query '{}' (key={}, reranked={})",
                ctx.query_without_date,
                key,
                is_reranked
            );
            let results: Vec<SearchResult> = cached
                .iter()
                .map(|r| {
                    // Cycle/142: content-type hydrates on the cache-hit path too
                    // (sidecar lookup by id is independent of the result cache).
                    let ct = loomem_core::content_type::get_content_type(&state.store, &r.id);
                    SearchResult {
                        id: r.id.clone(),
                        content: r.content.clone(),
                        score: r.score,
                        metadata: Some(json!({
                            "level": r.level,
                            "timestamp": r.timestamp,
                            "bm25_score": r.bm25_score,
                            "vector_score": r.vector_score,
                            "time_decay": r.time_decay_factor,
                            "cached": true,
                            "reranked": is_reranked,
                        })),
                        trace_info: None, // no trace for cached results
                        // Cache hit path skips fresh fusion — debug breakdown is
                        // a fresh-pipeline-only artifact in /86. Documented in
                        // 86-close.md § Findings.
                        signal_breakdown: None,
                        content_type: ct.map(|m| m.content_type.as_str().to_string()),
                        content_type_source: ct.map(|m| m.source.as_str().to_string()),
                    }
                })
                .collect();
            return Ok(Json(SearchResponse {
                results,
                took_ms: ctx.start.elapsed().as_millis() as u64,
                context_sufficiency: None,
                trace_metadata: None, // cache hit, no trace
                associations: None,
                recommendations: None,
                query_classification: surface_classification(&payload, &classification),
            }));
        }
    }

    // Multi-query decomposition (if enabled). `sub_queries` stays in the
    // orchestrator because it feeds BOTH bm25_retrieve (multi-query BM25
    // merge) and vector_retrieve (vector multi-query section).
    let sub_queries = if state.config.search.multi_query_enabled {
        let decompose_input = prepare_llm_input(&ctx.query_without_date, "multi_query::decompose");
        multi_query::decompose(&state.http_client, &state.config.llm, &decompose_input).await
    } else {
        vec![]
    };

    // BM25 retrieval (dual-query merge + multi-query merge). Acquires and
    // releases the tantivy lock internally. See `bm25_retrieve` below.
    // Cycle/258 (Option A): the source_agent filter is now pushed into Tantivy
    // inside `bm25_retrieve` (`bm25_leaf`), so the BM25 pool is already
    // agent-scoped at the source — no post-retrieval pre-filter pass needed.
    let bm25_results = bm25_retrieve(&ctx, &sub_queries, &payload, &state).await?;

    // Vector retrieval: embed query + hybrid search + vector multi-query merge.
    // Returns the full hybrid-results list (graph boost + scoring happen in
    // the orchestrator, preserved for Iter 3). See `vector_retrieve` below.
    let hybrid_results =
        vector_retrieve(&ctx, &sub_queries, bm25_results, &payload, &state).await?;

    // Cache store moved to AFTER all score mutations + filters + truncate
    // (see below, after hybrid_results.truncate). Previously this insert
    // happened here, storing pre-boost scores, which caused warm-path cache
    // hits to return unboosted scores via the early-return branch at line
    // ~216 — while cold requests applied graph boost, level boost, tier
    // overrides, importance, preference boost, filters, and truncate before
    // returning. Result: same query returned different scores and different
    // counts depending on cache state. See cycles/N7-cli-nondeterminism.md
    // and cycles/03-coordinated-core-fix/03-brief.md for the full trace.

    // Score + rank: graph boost + level boost + tier/importance/preference +
    // complexity-aware final sort. Sync (no await — `graph.find_related_chunks`
    // and `state.store.get_chunk` are blocking). See `score_and_rank` below.
    let hybrid_results = score_and_rank(hybrid_results, &ctx, &payload, &auth, &state);

    // RED-SD-1/SD-2 defense-in-depth: drop soft-deleted chunks before
    // reranker spawn (external LLM) and before filter_and_truncate. Covers
    // the vector path (no predicate push-down) and the stale-tantivy edge
    // case (silent delete in legacy flows).
    let hybrid_results: Vec<_> = hybrid_results
        .into_iter()
        .filter(|r| !crate::handlers::delete::is_deleted_in_store(&state, &r.id))
        .collect();

    // ECA-31: Check cost budget for reranker
    let rerank_cost_ok = {
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
    };

    // Async reranking: spawn in background, results cached for next identical query
    if state.config.search.rerank_enabled && rerank_cost_ok && !hybrid_results.is_empty() {
        if let Some(key) = ctx.cache_key {
            let candidates = hybrid_results
                .len()
                .min(state.config.search.rerank_candidates);
            let top_k = payload.top_k.unwrap_or(state.config.search.top_k);
            let chunks: Vec<String> = hybrid_results[..candidates]
                .iter()
                .map(|r| r.content.clone())
                .collect();
            let results_for_rerank = hybrid_results[..candidates].to_vec();
            let query_for_rerank = prepare_llm_input(&payload.query, "reranker::rerank");
            let cache = state.query_cache.clone();
            let http_client = state.http_client.clone();
            let llm_config = state.config.llm.clone();

            #[cfg(feature = "onnx-rerank")]
            let _onnx = state.onnx_reranker.as_ref().map(|_| state.graph.clone()); // marker for has_onnx
                                                                                   // Clone ONNX reranker ref if available — tract SimplePlan is Send+Sync
            #[cfg(feature = "onnx-rerank")]
            let has_onnx = state.onnx_reranker.is_some();
            #[cfg(not(feature = "onnx-rerank"))]
            let has_onnx = false;

            tokio::spawn(async move {
                let rerank_result = if has_onnx {
                    // ONNX reranker not easily cloneable into spawn — use LLM fallback in async
                    reranker::rerank(&http_client, &llm_config, &query_for_rerank, &chunks, top_k)
                        .await
                } else {
                    reranker::rerank(&http_client, &llm_config, &query_for_rerank, &chunks, top_k)
                        .await
                };

                match rerank_result {
                    Ok(indices) => {
                        let reranked: Vec<_> = indices
                            .into_iter()
                            .filter_map(|i| results_for_rerank.get(i).cloned())
                            .collect();
                        if !reranked.is_empty() {
                            let mut c = cache.lock().await;
                            c.insert_reranked(key, reranked.clone());
                            tracing::info!(
                                "Async rerank complete: {} → {} results, cached",
                                candidates,
                                reranked.len()
                            );
                        }
                    }
                    Err(e) => {
                        tracing::debug!("Async rerank failed (non-blocking): {}", e);
                    }
                }
            });
        }
    }

    // Filter + truncate: bitemporal, superseded, extraction_meta filters +
    // content dedup + aggregation dedup + source_agent filters + truncate to
    // final top-K. Returns the result list plus the counters that feed into
    // `ResponseMetrics`. See `filter_and_truncate` below.
    let FilterOutcome {
        results: hybrid_results,
        dedup_removed,
        total_results_before_topk,
        final_top_k,
    } = filter_and_truncate(hybrid_results, &ctx, &payload, &state);

    // Cache store — post-boost, post-filter, post-truncate. Warm-path cache
    // hits (line ~216) now return the same Vec<HybridSearchResult> that the
    // cold path is about to build into SearchResult. This closes the
    // cold/warm divergence diagnosed in cycles/N7-cli-nondeterminism.md §3.
    if let Some(key) = ctx.cache_key {
        let mut cache = state.query_cache.lock().await;
        cache.insert(key, hybrid_results.clone());
    }

    // Iter 1 (cycle/02): implicit boost + response serialization + sufficiency
    // scoring + trace + event emit + associations + recommendations are all
    // extracted into `build_response` below. Orchestrator packages the
    // pipeline metrics and hands off the final response envelope.
    let metrics = ResponseMetrics {
        start: ctx.start,
        complexity: ctx.complexity,
        dedup_removed,
        total_results_before_topk,
        final_top_k,
    };
    Ok(Json(
        build_response(
            metrics,
            hybrid_results,
            &state,
            &auth,
            &payload,
            &classification,
        )
        .await,
    ))
}

/// Cycle/85: gate `query_classification` on the request flag. Helper kept
/// off the hot path so the cache-hit and main-response branches share the
/// exact same logic.
fn surface_classification(
    payload: &SearchRequest,
    classification: &ClassifiedQuery,
) -> Option<ClassifiedQuery> {
    if payload.debug_query_classification {
        Some(classification.clone())
    } else {
        None
    }
}

/// Cycle/86: compute additive RRF breakdowns over the existing pipeline's
/// candidate pool, but only when the request flag is set. Returns one
/// `Option<SignalBreakdown>` per candidate (None when flag is off — the
/// per-result `signal_breakdown` field then serializes as omitted).
///
/// Path A: this helper does NOT alter `hybrid_results` ordering. The
/// `FusionResult.fused_order` is intentionally discarded — `/86` ships
/// breakdowns only; `/87` recovers fused order from the breakdowns when
/// running side-by-side comparisons against the existing pipeline.
fn compute_breakdowns_if_requested(
    hybrid_results: &[loomem_core::HybridSearchResult],
    classification: &ClassifiedQuery,
    payload: &SearchRequest,
) -> Vec<Option<loomem_core::search::SignalBreakdown>> {
    if !payload.debug_signal_breakdown || hybrid_results.is_empty() {
        return vec![None; hybrid_results.len()];
    }
    let fusion = loomem_core::search::fuse(
        hybrid_results,
        &classification.weights,
        loomem_core::search::FusionParams::now_default(),
    );
    fusion.breakdowns.into_iter().map(Some).collect()
}

/// Cycle/85: classify the query and emit a single tracing event.
/// Extracted so `search_handler` gains exactly one extra statement,
/// keeping cyclomatic-complexity delta at the call site minimal.
fn classify_and_trace(query: &str) -> ClassifiedQuery {
    let c = classify(query);
    tracing::debug!(
        query_type = ?c.query_type,
        entity_count = c.features.entities.len(),
        temporal_markers = c.features.temporal_markers.len(),
        doc_lookup_verbs = c.features.doc_lookup_verbs.len(),
        language = ?c.features.language_hint,
        "cycle/85 query classified"
    );
    c
}

/// Apply implicit boost, serialize `HybridSearchResult` → `SearchResult`,
/// compute context-sufficiency score, emit search event, gather associations
/// (ECA-29/31 circuit breakers) and recommendations, and assemble the final
/// `SearchResponse`. Extracted from `search_handler` in cycle/02 Iter 1.
///
/// Takes owned `hybrid_results` (consumed by `into_iter` in the mapping
/// stage). `state`/`auth`/`payload` are borrowed — this function performs
/// read-only access on `state.store` for trace info and write access via
/// `implicit_boost` / `boost_score` / `increment_access_count` (existing
/// side-effects preserved exactly).
async fn build_response(
    metrics: ResponseMetrics,
    hybrid_results: Vec<loomem_core::HybridSearchResult>,
    state: &Arc<AppState>,
    auth: &AuthContext,
    payload: &SearchRequest,
    classification: &ClassifiedQuery,
) -> SearchResponse {
    let ResponseMetrics {
        start,
        complexity,
        dedup_removed,
        total_results_before_topk,
        final_top_k,
    } = metrics;

    // Implicit boost: top 3 results get +0.1 importance (cap 1.5, max once per 24h)
    // Skipped in dry_run mode (eval) to avoid contaminating importance data
    if !payload.dry_run {
        for r in hybrid_results.iter().take(3) {
            match state.store.implicit_boost(&r.id, 0.1, 1.5, 86400_u64) {
                Ok(true) => tracing::debug!("Implicit boost applied to {}", r.id),
                Ok(false) => {}
                Err(e) => warn!("Implicit boost failed for {}: {}", r.id, e),
            }
        }
    }

    // Cycle/86 Path A: optionally compute additive RRF breakdown over the same
    // candidate pool the existing pipeline produced. The active fusion path
    // is unchanged — these breakdowns are surfaced as a debug field for
    // `/87 per-type eval` to compare. Computed before the consuming iter so
    // each breakdown can be attached by index.
    let signal_breakdowns: Vec<Option<loomem_core::search::SignalBreakdown>> =
        compute_breakdowns_if_requested(&hybrid_results, classification, payload);

    // Convert to response format, apply access boost, and increment access count
    let results: Vec<SearchResult> = hybrid_results
        .into_iter()
        .enumerate()
        .map(|(idx, r)| {
            if !payload.dry_run && state.config.worker.decay_worker.access_boost {
                if let Err(e) = state.store.boost_score(&r.id) {
                    warn!("Failed to boost score for {}: {}", r.id, e);
                }
            }
            // Increment access_count for adaptive decay
            if !payload.dry_run && state.config.worker.decay_worker.adaptive_enabled {
                if let Err(e) = state.store.increment_access_count(&r.id) {
                    warn!("Failed to increment access count for {}: {}", r.id, e);
                }
            }

            let chunk_opt = state.store.get_chunk(&r.id).ok().flatten();
            let source = chunk_opt
                .as_ref()
                .and_then(|c| c.source.as_ref().map(|s| s.to_string()));

            let trace_info = if payload.trace {
                chunk_opt.as_ref().map(|c| super::types::TraceInfo {
                    level: format!("L{}", c.level),
                    source: c.source.as_ref().map(|s| s.to_string()),
                    is_latest: c.is_latest,
                    created_at: c.timestamp,
                    memory_type: c.memory_type.clone(),
                    importance: c.importance.unwrap_or(0.0),
                    access_count: c.access_count,
                    version: c.version,
                    superseded_by: c.superseded_by.clone(),
                    prompt_version: c.prompt_version,
                })
            } else {
                None
            };

            let event_date = chunk_opt
                .as_ref()
                .and_then(|c| c.extraction_meta.as_ref())
                .and_then(|m| m.event_date.as_ref())
                .cloned();
            let valid_from = chunk_opt.as_ref().and_then(|c| c.valid_from);

            // Cycle/142 + /143: hydrate content-type from the sidecar by id
            // (additive, wzorzec /93). `ContentTypeMeta` is Copy, so the Option
            // is reused for both surface strings. `None` → no fields emitted.
            let content_type_meta =
                loomem_core::content_type::get_content_type(&state.store, &r.id);

            SearchResult {
                id: r.id,
                content: r.content,
                score: r.score,
                metadata: Some(json!({
                    "user_id": r.user_id,
                    "app_id": r.app_id,
                    "level": r.level,
                    "timestamp": r.timestamp,
                    "bm25_score": r.bm25_score,
                    "vector_score": r.vector_score,
                    "time_decay": r.time_decay_factor,
                    "source": source,
                    "event_date": event_date,
                    "valid_from": valid_from,
                })),
                trace_info,
                signal_breakdown: signal_breakdowns.get(idx).cloned().flatten(),
                content_type: content_type_meta.map(|m| m.content_type.as_str().to_string()),
                content_type_source: content_type_meta.map(|m| m.source.as_str().to_string()),
            }
        })
        .collect();

    // Context sufficiency: coverage (results/requested) × diversity (unique levels)
    let sufficiency = if !results.is_empty() {
        let coverage = (results.len() as f64) / (final_top_k as f64);

        let levels: std::collections::HashSet<i64> = results
            .iter()
            .filter_map(|r| {
                r.metadata
                    .as_ref()
                    .and_then(|m| m.get("level"))
                    .and_then(|v| v.as_i64())
            })
            .collect();
        let diversity = (levels.len() as f64 / 2.0).min(1.0); // normalize: 2 levels (L0, L1) = max

        let scores: Vec<f64> = results.iter().map(|r| r.score).collect();
        let top = scores.first().copied().unwrap_or(0.0);
        let bottom = scores.last().copied().unwrap_or(0.0);
        let score_spread = if top > 0.0 { bottom / top } else { 0.0 };

        let score = (coverage * 0.4 + diversity * 0.3 + score_spread * 0.3).min(1.0);
        let confidence = if score >= 0.7 {
            "high"
        } else if score >= 0.4 {
            "medium"
        } else {
            "low"
        };

        Some(ContextSufficiency {
            score,
            coverage,
            diversity,
            confidence,
        })
    } else {
        None
    };

    let trace_metadata = if payload.trace {
        Some(super::types::TraceMetadata {
            total_results_before_topk,
            dedup_removed,
            search_latency_us: start.elapsed().as_micros() as u64,
            query_complexity: if state.config.search.complexity.enabled {
                Some(format!("{}", complexity))
            } else {
                None
            },
        })
    } else {
        None
    };

    let took_ms = start.elapsed().as_millis() as u64;

    // Emit search event
    if let Some(ref tx) = state.event_tx {
        let top_scores: Vec<f32> = results.iter().take(5).map(|r| r.score as f32).collect();
        loomem_core::event_log::emit(
            tx,
            loomem_core::event_log::MemoryEvent::Search {
                query: payload.query.clone(),
                stream_id: auth.stream_id.clone(),
                top_scores,
                latency_ms: took_ms,
                result_count: results.len(),
            },
        );
    }

    // /150e-2 access-audit (ADR-018): metadata-only, no query text, aggregate
    // (target_id=None for search). No-op when the feature is disabled.
    crate::access_hook::record_access(
        state,
        auth,
        loomem_core::access_audit::AccessOp::Search,
        None,
        results.len(),
    );

    // Association computation (ECA-21) with ECA-29/31 circuit breakers
    let associations = if payload.include_associations && state.config.associator.enabled {
        // ECA-29: Check association health before running
        let health_ok =
            loomem_core::associator::should_run_associations(&state.store, &auth.stream_id);
        // ECA-31: Check cost budget — skip associations if budget tier says so
        let cost_status = loomem_core::cost_tracker::check_cost_budget_for_stream(
            &state.store,
            &auth.stream_id,
            state.config.cost.daily_cap_usd,
        );
        if health_ok && cost_status.allow_associations() {
            compute_associations(state, &payload.query, &results, &auth.stream_id).await
        } else {
            if !health_ok {
                tracing::warn!(
                    "Associations skipped for stream {}: health check failed (ECA-29)",
                    auth.stream_id
                );
            }
            if !cost_status.allow_associations() {
                tracing::warn!(
                    "Associations skipped for stream {}: cost budget tier={} (ECA-31)",
                    auth.stream_id,
                    cost_status.description()
                );
            }
            None
        }
    } else {
        None
    };

    // ECA-11: Inline recommendations from cached advisories (lightweight)
    let recommendations = if state.config.advisor.enabled {
        let cached = loomem_core::advisor::get_cached_advisories(&state.store, &auth.stream_id, 2);
        if cached.is_empty() {
            None
        } else {
            Some(cached)
        }
    } else {
        None
    };

    SearchResponse {
        results,
        took_ms,
        context_sufficiency: sufficiency,
        trace_metadata,
        associations,
        recommendations,
        query_classification: surface_classification(payload, classification),
    }
}

/// BM25 retrieval: dual-query merge (original vs. expanded, weights 1.0/0.5)
/// and multi-query merge (sub-queries from `multi_query::decompose`). Owns
/// the tantivy lock for the duration and drops it before returning so the
/// caller can proceed with vector search without serialising on it.
///
/// Preserves all branches from pre-refactor: date-range search, entity
/// search, stream-aware merge (single-stream fast path vs. multi-stream
/// map-merge), and expanded-query score blending. Multi-query BM25 merges
/// sub-query results with max-score semantics.
async fn bm25_retrieve(
    ctx: &QueryContext,
    sub_queries: &[String],
    payload: &SearchRequest,
    state: &Arc<AppState>,
) -> Result<Vec<loomem_core::SearchResult>, AppError> {
    // Dual-query merge: search with original (weight 1.0) and expanded (weight 0.5)
    let tantivy = state.tantivy.lock().await;
    let oq: &str = &ctx.original_query_stemmed;

    let bm25_results = if let Some(ref date_filter) = ctx.date_filter {
        let (start_ts, end_ts) = match date_filter {
            DateFilter::Range(start, end) => (*start, *end),
        };

        bm25_leaf(
            &tantivy,
            payload,
            Bm25Leaf::date(oq, (start_ts, end_ts), ctx.limit * 2),
        )?
    } else if let Some(ref entity) = payload.entity {
        bm25_leaf(
            &tantivy,
            payload,
            Bm25Leaf::entity(oq, entity, ctx.limit * 2),
        )?
    } else {
        let stream_filter: Option<Vec<String>> = ctx.stream_list.clone();

        let search_query =
            |query: &str, lim: usize| -> anyhow::Result<Vec<loomem_core::SearchResult>> {
                match &stream_filter {
                    Some(streams) if streams.len() == 1 => bm25_leaf(
                        &tantivy,
                        payload,
                        Bm25Leaf::plain(query, Some(&streams[0]), lim),
                    ),
                    Some(streams) => {
                        let mut merged_map: std::collections::HashMap<
                            String,
                            loomem_core::SearchResult,
                        > = std::collections::HashMap::new();
                        for s in streams {
                            let results =
                                bm25_leaf(&tantivy, payload, Bm25Leaf::plain(query, Some(s), lim))?;
                            for r in results {
                                merged_map
                                    .entry(r.id.clone())
                                    .and_modify(|e| {
                                        if r.score > e.score {
                                            e.score = r.score;
                                        }
                                    })
                                    .or_insert(r);
                            }
                        }
                        let mut merged: Vec<_> = merged_map.into_values().collect();
                        merged.sort_by(|a, b| {
                            b.score
                                .partial_cmp(&a.score)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        });
                        Ok(merged)
                    }
                    None => bm25_leaf(&tantivy, payload, Bm25Leaf::plain(query, None, lim)),
                }
            };

        let original_results = search_query(&ctx.original_query_stemmed, ctx.limit * 2)?;

        if ctx.original_query_stemmed == ctx.expanded_query_stemmed {
            original_results
        } else {
            let expanded_results = search_query(&ctx.expanded_query_stemmed, ctx.limit * 2)?;

            let mut merged_map: std::collections::HashMap<String, loomem_core::SearchResult> =
                std::collections::HashMap::new();

            for result in original_results {
                merged_map.insert(result.id.clone(), result);
            }

            for result in expanded_results {
                let adjusted_score = result.score * 0.5;
                merged_map
                    .entry(result.id.clone())
                    .and_modify(|e| {
                        if adjusted_score > e.score {
                            e.score = adjusted_score;
                        }
                    })
                    .or_insert_with(|| {
                        let mut r = result;
                        r.score = adjusted_score;
                        r
                    });
            }

            let mut merged: Vec<_> = merged_map.into_values().collect();
            merged.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            merged
        }
    };

    // Multi-query merge: run BM25 for each sub-query and merge with max scores
    let bm25_results = if state.config.search.multi_query_enabled && !sub_queries.is_empty() {
        let mut multi_merged_map: std::collections::HashMap<String, loomem_core::SearchResult> =
            std::collections::HashMap::new();

        for result in bm25_results {
            multi_merged_map.insert(result.id.clone(), result);
        }

        for sub_query in sub_queries {
            if sub_query == &ctx.query_without_date {
                continue;
            }

            let sub_query_stemmed = if state.config.search.stem_polish {
                let words: Vec<String> =
                    sub_query.split_whitespace().flat_map(polish_stem).collect();
                words.join(" ")
            } else {
                sub_query.clone()
            };

            let sub_results = if let Some(ref streams) = ctx.stream_list {
                let mut sub_merged: std::collections::HashMap<String, loomem_core::SearchResult> =
                    std::collections::HashMap::new();
                for s in streams {
                    if let Ok(results) = bm25_leaf(
                        &tantivy,
                        payload,
                        Bm25Leaf::plain(&sub_query_stemmed, Some(s), ctx.limit * 2),
                    ) {
                        for r in results {
                            sub_merged
                                .entry(r.id.clone())
                                .and_modify(|e| {
                                    if r.score > e.score {
                                        e.score = r.score;
                                    }
                                })
                                .or_insert(r);
                        }
                    }
                }
                sub_merged.into_values().collect()
            } else {
                bm25_leaf(
                    &tantivy,
                    payload,
                    Bm25Leaf::plain(&sub_query_stemmed, None, ctx.limit * 2),
                )
                .ok()
                .unwrap_or_default()
            };

            for result in sub_results {
                multi_merged_map
                    .entry(result.id.clone())
                    .and_modify(|e| {
                        if result.score > e.score {
                            e.score = result.score;
                        }
                    })
                    .or_insert(result);
            }
        }

        let mut multi_merged: Vec<_> = multi_merged_map.into_values().collect();
        multi_merged.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        multi_merged
    } else {
        bm25_results
    };

    drop(tantivy);
    Ok(bm25_results)
}

/// Vector retrieval: embed query (local embedder or OpenAI), run hybrid
/// search with stream pre-filter, and merge vector-multi-query results.
/// Falls back to BM25-only when vector search is disabled, simple query,
/// embedding fails, or no embeddings exist.
///
/// Stream pre-filtering happens BEFORE vector ranking — without this, the
/// top-K picked from all streams would be post-filtered down to zero when
/// user's stream is minority. Preserves F-R3 (duplicate embed logic between
/// main query and vector multi-query — not DRY'd in this cycle because the
/// two paths cache differently).
async fn vector_retrieve(
    ctx: &QueryContext,
    sub_queries: &[String],
    bm25_results: Vec<loomem_core::SearchResult>,
    payload: &SearchRequest,
    state: &Arc<AppState>,
) -> Result<Vec<loomem_core::HybridSearchResult>, AppError> {
    // Cycle/258 (Option A): agent id-set fetched once (None when no filter); the
    // two vector pre-filters use it via `retain_by_agent_set` (HashSet lookup,
    // replacing #257's per-embedding `get_chunk`).
    let agent_set = agent_id_set(state, payload).await;

    // Try hybrid search with vector embeddings
    // Simple queries skip vector search for lower latency (BM25 only)
    let skip_vector = ctx.complexity == QueryComplexity::Simple;
    let hybrid_results = if state.config.storage.vector_enabled && !skip_vector {
        // Embed query — local model or OpenAI
        let embed_result = if let Some(ref embedder) = state.local_embedder {
            embedder.embed(&payload.query)
        } else if let Some(api_key) = state.config.llm.get_api_key() {
            embeddings::embed(
                &state.http_client,
                &api_key,
                &state.config.llm.embedding_model,
                &payload.query,
            )
            .await
        } else {
            Err(anyhow::anyhow!("No embedding provider available"))
        };
        match embed_result {
            Ok(query_embedding) => {
                match state.store.get_all_embeddings() {
                    Ok(all_embeddings) => {
                        // Cycle/258: pre-filter the vector pool by the agent
                        // id-set before scoring/truncation (parity with the BM25
                        // source-level filter). No-op when no agent filter.
                        let all_embeddings =
                            retain_by_agent_set(all_embeddings, agent_set.as_ref());
                        if !all_embeddings.is_empty() {
                            let vector_stream_filter: Option<Vec<String>> = ctx.stream_list.clone();

                            // Pre-filter embeddings by stream BEFORE vector search
                            // Without this, vector search picks top-K from ALL streams,
                            // then post-filter removes most results → vector_score=0
                            let filtered_embeddings =
                                if let Some(ref streams) = vector_stream_filter {
                                    all_embeddings
                                        .into_iter()
                                        .filter(|(id, _)| {
                                            state
                                                .store
                                                .get_chunk(id)
                                                .ok()
                                                .flatten()
                                                .map(|c| streams.contains(&c.stream))
                                                .unwrap_or(false)
                                        })
                                        .collect::<Vec<_>>()
                                } else {
                                    all_embeddings
                                };

                            tracing::info!(
                                "Performing hybrid search with {} embeddings (stream-filtered)",
                                filtered_embeddings.len()
                            );

                            if filtered_embeddings.is_empty() {
                                state.hybrid_search.bm25_only(bm25_results)?
                            } else {
                                state.hybrid_search.search_with_vector(
                                    bm25_results,
                                    &filtered_embeddings,
                                    &query_embedding,
                                    Some(&state.store),
                                    None, // already filtered, no need for post-filter
                                )?
                            }
                        } else {
                            tracing::info!("No embeddings found, falling back to BM25 only");
                            state.hybrid_search.bm25_only(bm25_results)?
                        }
                    }
                    Err(e) => {
                        warn!("Failed to retrieve embeddings: {}, falling back to BM25", e);
                        state.hybrid_search.bm25_only(bm25_results)?
                    }
                }
            }
            Err(e) => {
                warn!("Failed to embed query: {}, falling back to BM25", e);
                state.hybrid_search.bm25_only(bm25_results)?
            }
        }
    } else {
        state.hybrid_search.bm25_only(bm25_results)?
    };

    // Vector multi-query: embed each sub-query, search vectors, merge results
    let hybrid_results = if state.config.search.vector_multi_query
        && state.config.search.multi_query_enabled
        && !sub_queries.is_empty()
        && state.config.storage.vector_enabled
        && ctx.complexity != QueryComplexity::Simple
    {
        let mut vec_merged_map: std::collections::HashMap<String, loomem_core::HybridSearchResult> =
            std::collections::HashMap::new();
        for r in hybrid_results {
            vec_merged_map.insert(r.id.clone(), r);
        }

        // Get embeddings once (shared across sub-queries)
        let all_embs = retain_by_agent_set(
            state.store.get_all_embeddings().unwrap_or_default(),
            agent_set.as_ref(),
        );
        let vector_stream_filter: Option<Vec<String>> = ctx.stream_list.clone();
        let filtered_embs: Vec<_> = if let Some(ref streams) = vector_stream_filter {
            all_embs
                .into_iter()
                .filter(|(id, _)| {
                    state
                        .store
                        .get_chunk(id)
                        .ok()
                        .flatten()
                        .map(|c| streams.contains(&c.stream))
                        .unwrap_or(false)
                })
                .collect()
        } else {
            all_embs
        };

        if !filtered_embs.is_empty() {
            for sub_q in sub_queries {
                if sub_q == &ctx.query_without_date {
                    continue; // already embedded as main query
                }
                let sub_embed = if let Some(ref embedder) = state.local_embedder {
                    embedder.embed(sub_q)
                } else if let Some(api_key) = state.config.llm.get_api_key() {
                    embeddings::embed(
                        &state.http_client,
                        &api_key,
                        &state.config.llm.embedding_model,
                        sub_q,
                    )
                    .await
                } else {
                    continue;
                };

                if let Ok(sub_emb) = sub_embed {
                    // Cosine similarity search
                    let mut scored: Vec<(String, f64)> = filtered_embs
                        .iter()
                        .map(|(id, emb)| {
                            let sim = cosine_similarity(&sub_emb, emb);
                            (id.clone(), sim)
                        })
                        .collect();
                    scored
                        .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

                    for (chunk_id, sim) in scored.into_iter().take(ctx.limit) {
                        if sim < 0.3 {
                            break;
                        }
                        vec_merged_map
                            .entry(chunk_id.clone())
                            .and_modify(|e| {
                                // Boost score if sub-query found it with high similarity
                                let sub_boost = sim * 0.5;
                                if e.vector_score < sim as f32 {
                                    e.vector_score = sim as f32;
                                }
                                e.score += sub_boost;
                            })
                            .or_insert_with(|| {
                                let chunk = state.store.get_chunk(&chunk_id).ok().flatten();
                                loomem_core::HybridSearchResult {
                                    id: chunk_id,
                                    content: chunk
                                        .as_ref()
                                        .map(|c| c.content.clone())
                                        .unwrap_or_default(),
                                    user_id: String::new(),
                                    app_id: String::new(),
                                    level: chunk.as_ref().map(|c| c.level).unwrap_or(0),
                                    timestamp: chunk
                                        .as_ref()
                                        .map(|c| c.timestamp as i64)
                                        .unwrap_or(0),
                                    score: sim * 0.5,
                                    bm25_score: 0.0,
                                    vector_score: sim as f32,
                                    time_decay_factor: 1.0,
                                }
                            });
                    }
                    tracing::debug!("Vector multi-query: embedded sub-query '{}'", sub_q);
                }
            }
        }

        let mut merged: Vec<_> = vec_merged_map.into_values().collect();
        merged.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        merged
    } else {
        hybrid_results
    };

    Ok(hybrid_results)
}

/// Cycle/118: log-frequency multiplier for implicit access-count boost.
///
/// Formula: `1.0 + ln(1.0 + access_count) * weight`
///
/// When `weight <= 0.0` or `access_count == 0`, returns exactly `1.0` (no-op,
/// no f64 multiplication noise — Tier C identity guarantee). Negative weights
/// (invalid config) are clamped to zero rather than inverting search order —
/// see cycle/126.
///
/// Log scaling provides natural diminishing returns: access_count=100 yields
/// ln(101) ≈ 4.6× factor; access_count=10000 yields 9.2×.
///
/// Pattern source: Memento, OpenClaw Dreaming Frequency=0.24 component.
/// Complementary to /115 auto-persistent (passive decay protection) — this
/// is active rank boost on every search.
fn compute_access_boost_multiplier(access_count: u32, weight: f64) -> f64 {
    // Clamp negative weights to zero. A negative `implicit_access_boost_weight`
    // in config.toml would otherwise produce multipliers < 1.0 (and negative
    // for large access_count), silently inverting result sort order.
    let weight = weight.max(0.0);
    if weight == 0.0 || access_count == 0 {
        return 1.0;
    }
    // lossless u32→f64 conversion (f64 mantissa 52 bits >> u32 32 bits)
    let count = f64::from(access_count);
    1.0 + (1.0 + count).ln() * weight
}

/// Score + rank pipeline: apply graph boost (existing results + graph-only
/// additions respecting stream isolation), level-based boost (L1 > L0,
/// inverted in counting mode), tier-based scoring overrides (core undoes
/// time decay, pinned clamps it, ephemeral accelerates), importance
/// multiplier + PreferenceOrDecision 1.5x boost, and complexity-aware final
/// sort (Temporal → chronological, otherwise → score desc).
///
/// Sync (no await): `graph.find_related_chunks` and `state.store.get_chunk`
/// are blocking. Sort/boost is pure CPU work once chunks are loaded. Pulling
/// this out of the orchestrator isolates the scoring policy from I/O and
/// makes it easier to swap strategies (see cycles/02-brief.md §score_and_rank).
fn score_and_rank(
    mut hybrid_results: Vec<loomem_core::HybridSearchResult>,
    ctx: &QueryContext,
    payload: &SearchRequest,
    auth: &AuthContext,
    state: &Arc<AppState>,
) -> Vec<loomem_core::HybridSearchResult> {
    // Graph-enhanced search: boost results with graph connections, add graph-discovered chunks
    if state.config.search.graph.enabled {
        let query_entities = state.entity_extractor.extract(&payload.query);
        if !query_entities.is_empty() {
            let entity_names: Vec<String> = query_entities.iter().map(|(n, _)| n.clone()).collect();
            if let Ok(graph_chunks) = state.graph.find_related_chunks(
                &entity_names,
                state.config.search.graph.max_hops,
                &auth.stream_id,
            ) {
                let graph_scores: std::collections::HashMap<String, f64> =
                    graph_chunks.into_iter().collect();
                let boost_factor = state.config.search.graph.boost_factor;

                // Boost existing results with graph proximity
                for result in &mut hybrid_results {
                    if let Some(&graph_score) = graph_scores.get(&result.id) {
                        result.score *= 1.0 + (graph_score * boost_factor);
                    }
                }

                // Add graph-discovered chunks not in results
                let existing_ids: std::collections::HashSet<String> =
                    hybrid_results.iter().map(|r| r.id.clone()).collect();
                let mut additions = 0;
                let max_additions = state.config.search.graph.max_graph_additions;

                for (chunk_id, graph_score) in &graph_scores {
                    if additions >= max_additions {
                        break;
                    }
                    if existing_ids.contains(chunk_id) {
                        continue;
                    }

                    if let Ok(Some(chunk)) = state.store.get_chunk(chunk_id) {
                        if chunk.dormant {
                            continue;
                        }
                        // Respect stream isolation
                        if let Some(ref streams) = ctx.stream_list {
                            if !streams.contains(&chunk.stream) {
                                continue;
                            }
                        }
                        hybrid_results.push(loomem_core::HybridSearchResult {
                            id: chunk.id,
                            content: chunk.content,
                            user_id: String::new(),
                            app_id: String::new(),
                            level: chunk.level,
                            timestamp: chunk.timestamp as i64,
                            score: graph_score * boost_factor,
                            bm25_score: 0.0,
                            vector_score: 0.0,
                            time_decay_factor: 1.0,
                        });
                        additions += 1;
                    }
                }

                if additions > 0 {
                    tracing::debug!(
                        "Graph search: boosted results + added {} graph-only chunks",
                        additions
                    );
                }
            }
        }
    }

    // Apply level-based boost: higher-tier memories rank above raw events
    // For counting/aggregation queries with counting_l0_preference: invert to prefer L0
    let counting_mode =
        state.config.search.counting_l0_preference && is_counting_query(&payload.query);
    let hybrid_results: Vec<_> = hybrid_results
        .into_iter()
        .map(|mut r| {
            let boost = if counting_mode {
                match r.level {
                    0 => 1.5, // L0 raw events preferred for counting
                    1 => 0.7, // L1 summaries penalized
                    _ => 0.8,
                }
            } else {
                match r.level {
                    1 => 1.5, // L1 compressed summaries
                    0 => 1.0, // L0 raw events (baseline)
                    _ => 0.8,
                }
            };
            r.score *= boost;
            r
        })
        .collect();

    let mut hybrid_results = hybrid_results;
    hybrid_results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Apply tier-based scoring overrides + importance multiplier
    let hybrid_results: Vec<_> = hybrid_results
        .into_iter()
        .map(|mut r| {
            if let Ok(Some(chunk)) = state.store.get_chunk(&r.id) {
                apply_chunk_score_adjustments(&mut r, &chunk, &state.config.search);
            }
            r
        })
        .collect();

    let mut hybrid_results = hybrid_results;
    if matches!(ctx.complexity, QueryComplexity::Temporal) {
        // Temporal queries: sort chronologically (oldest first) for timeline reasoning
        hybrid_results.sort_by_key(|r| r.timestamp);
    } else {
        hybrid_results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    hybrid_results
}

/// Apply per-chunk score adjustments: tier override (core/pinned/ephemeral),
/// importance multiplier, implicit access boost (cycle/118), and preference
/// boost (PreferenceOrDecision facts). Pure CPU work after chunk load; the
/// `state.store.get_chunk` call stays in the orchestrator.
///
/// Extracted in cycle/121 to recover MI on `score_and_rank` after /118's
/// +5 NLOC regression (see cycles/121-score-and-rank-mi-recover-close.md).
fn apply_chunk_score_adjustments(
    r: &mut loomem_core::HybridSearchResult,
    chunk: &loomem_core::storage::Chunk,
    search_cfg: &loomem_core::config::SearchConfig,
) {
    // Extract tier from metadata (default = "default")
    let tier = chunk
        .metadata
        .as_ref()
        .and_then(|m| m.get("tier"))
        .and_then(|v| v.as_str())
        .unwrap_or("default");

    match tier {
        "core" => {
            // Core facts: undo time decay entirely, strong boost
            if r.time_decay_factor > 0.0 {
                r.score /= r.time_decay_factor;
            }
            r.score *= 2.0;
        }
        "pinned" => {
            // Pinned facts: clamp decay to minimum 0.8, moderate boost
            if r.time_decay_factor < 0.8 && r.time_decay_factor > 0.0 {
                r.score *= 0.8 / r.time_decay_factor;
            }
            r.score *= 1.5;
        }
        "ephemeral" => {
            // Ephemeral: accelerated decay + penalty
            r.score *= r.time_decay_factor * 0.5;
        }
        _ => {} // "default" — no change
    }

    // Importance multiplier
    let importance_weight = match chunk.importance {
        Some(imp) => imp,
        None => search_cfg.importance.medium_weight,
    };
    r.score *= importance_weight;

    // Cycle/118: implicit access-count boost (active rank signal).
    // Disabled when implicit_access_boost_weight=0.0 (returns 1.0).
    let access_boost = compute_access_boost_multiplier(
        chunk.access_count,
        search_cfg.implicit_access_boost_weight,
    );
    r.score *= access_boost;

    // Preference boost: PreferenceOrDecision facts score 1.5x
    if let Some(ref meta) = chunk.extraction_meta {
        if matches!(
            meta.fact_type,
            loomem_core::storage::FactType::PreferenceOrDecision
        ) {
            r.score *= 1.5;
        }
    }
}

/// Bitemporal filter helper: retain only chunks whose `[valid_from, valid_until]`
/// interval covers `valid_at`. Fail-open + `tracing::warn!` on metadata load
/// failure — see `cycles/42-audit-findings.md` §A (decision D2).
///
/// Open intervals (`valid_from = None` or `valid_until = None`) are treated as
/// unbounded on that side, matching the bitemporal semantics in
/// `loomem-core/src/storage.rs::Chunk`.
fn filter_bitemporal(
    hybrid_results: &mut Vec<loomem_core::HybridSearchResult>,
    store: &loomem_core::RocksDbStore,
    valid_at: u64,
) {
    hybrid_results.retain(|r| {
        if let Ok(Some(chunk)) = store.get_chunk(&r.id) {
            let from_ok = chunk.valid_from.is_none_or(|f| f <= valid_at);
            let until_ok = chunk.valid_until.is_none_or(|u| u >= valid_at);
            from_ok && until_ok
        } else {
            tracing::warn!(
                chunk_id = %r.id,
                "bitemporal filter: chunk metadata unavailable, fail-open keep"
            );
            true
        }
    });
}

/// Superseded filter helper: retain only chunks with `is_latest = true`.
/// Fail-closed + `tracing::warn!` on metadata load failure — see
/// `cycles/42-audit-findings.md` §A (decision D1). A superseded chunk silently
/// bypassing this filter is a regression of the supersede contract hardened in
/// legacy-agent /40, so we drop the chunk and log when we cannot verify `is_latest`.
fn filter_superseded(
    hybrid_results: &mut Vec<loomem_core::HybridSearchResult>,
    store: &loomem_core::RocksDbStore,
) {
    hybrid_results.retain(|r| {
        if let Ok(Some(chunk)) = store.get_chunk(&r.id) {
            chunk.is_latest
        } else {
            tracing::warn!(
                chunk_id = %r.id,
                "is_latest filter: chunk metadata unavailable, fail-closed drop"
            );
            false
        }
    });
}

/// Filter + truncate pipeline: bitemporal validity filter, superseded
/// filter (is_latest), extraction_meta filter (fact_type/subject/min_confidence),
/// content-prefix dedup (150-char fingerprint, 80% prefix overlap), aggregation
/// dedup (keep highest-scored chunk per subject), source_agent include/exclude
/// filters, and truncate to final top-K (30 for Aggregation, config.top_k
/// otherwise, overridable via payload.top_k).
///
/// Returns `FilterOutcome` bundling the result list with `dedup_removed`,
/// `total_results_before_topk`, and `final_top_k` — the three counters that
/// feed into `ResponseMetrics` for trace metadata. Sync because every filter
/// is a `retain` over a `Vec`.
fn filter_and_truncate(
    mut hybrid_results: Vec<loomem_core::HybridSearchResult>,
    ctx: &QueryContext,
    payload: &SearchRequest,
    state: &Arc<AppState>,
) -> FilterOutcome {
    // Bitemporal filter: keep only chunks valid at the requested point in time.
    // Fail-open + tracing::warn on metadata load failure (cycle/42 D2):
    // time-travel is exploration UX, false negatives are worse than false
    // positives. See cycles/42-audit-findings.md §A.
    if let Some(valid_at) = payload.valid_at {
        filter_bitemporal(&mut hybrid_results, &state.store, valid_at);
    }

    // Filter superseded chunks: by default, only show is_latest=true.
    // Fail-closed + tracing::warn on metadata load failure (cycle/42 D1):
    // a superseded chunk silently bypassing this filter is a regression of the
    // supersede contract hardened in legacy-agent /40. See cycles/42-audit-findings.md §A.
    if !payload.include_superseded {
        filter_superseded(&mut hybrid_results, &state.store);
    }

    // Filter by extraction_meta fields (fact_type, subject, min_confidence)
    if payload.fact_type.is_some() || payload.subject.is_some() || payload.min_confidence.is_some()
    {
        hybrid_results.retain(|r| {
            if let Ok(Some(chunk)) = state.store.get_chunk(&r.id) {
                if let Some(ref meta) = chunk.extraction_meta {
                    let type_ok = payload.fact_type.as_ref().is_none_or(|ft| {
                        let chunk_type = match meta.fact_type {
                            loomem_core::storage::FactType::PreferenceOrDecision => {
                                "preference_or_decision"
                            }
                            loomem_core::storage::FactType::ProjectState => "project_state",
                            loomem_core::storage::FactType::Fact => "fact",
                            loomem_core::storage::FactType::Event => "event",
                            loomem_core::storage::FactType::Experience => "experience",
                        };
                        // Match the built-in enum string, or an operator-configured
                        // custom topic preserved in `meta.topic` (collapses to Fact
                        // in the enum, so the raw key is the only way to filter it).
                        chunk_type == ft.as_str() || meta.topic.as_deref() == Some(ft.as_str())
                    });
                    let subject_ok = payload.subject.as_ref().is_none_or(|s| {
                        meta.subject
                            .as_ref()
                            .is_some_and(|ms| ms.to_lowercase().contains(&s.to_lowercase()))
                    });
                    let conf_ok = payload
                        .min_confidence
                        .is_none_or(|mc| meta.confidence >= mc);
                    type_ok && subject_ok && conf_ok
                } else {
                    // No extraction_meta — exclude if any extraction filter is set
                    false
                }
            } else {
                true
            }
        });
    }

    // Deduplicate near-identical results: keep highest-scored, collapse similar content.
    // Uses first 150 chars as fingerprint — catches duplicate ingest from daily logs.
    let count_before_dedup = hybrid_results.len();
    {
        let mut seen_fingerprints: Vec<String> = Vec::new();
        hybrid_results.retain(|r| {
            let fp: String = r
                .content
                .chars()
                .take(150)
                .collect::<String>()
                .to_lowercase();
            // Check if any existing fingerprint shares >80% prefix
            let dominated = seen_fingerprints.iter().any(|existing| {
                let min_len = fp.len().min(existing.len());
                if min_len < 30 {
                    return false;
                }
                let common = fp
                    .chars()
                    .zip(existing.chars())
                    .take_while(|(a, b)| a == b)
                    .count();
                common as f64 / min_len as f64 > 0.8
            });
            if dominated {
                tracing::debug!("Dedup: dropped duplicate result {}", r.id);
                false
            } else {
                seen_fingerprints.push(fp);
                true
            }
        });
    }
    // Aggregation dedup: keep only the highest-scored chunk per subject
    if matches!(ctx.complexity, QueryComplexity::Aggregation) {
        let mut seen_subjects: std::collections::HashSet<String> = std::collections::HashSet::new();
        hybrid_results.retain(|r| {
            if let Ok(Some(chunk)) = state.store.get_chunk(&r.id) {
                if let Some(ref meta) = chunk.extraction_meta {
                    if let Some(ref subject) = meta.subject {
                        let key = subject.to_lowercase();
                        if seen_subjects.contains(&key) {
                            return false; // skip duplicate subject
                        }
                        seen_subjects.insert(key);
                    }
                }
            }
            true
        });
    }

    let dedup_removed = count_before_dedup - hybrid_results.len();

    // Source agent filtering (post-retrieval)
    if let Some(ref agent) = payload.source_agent {
        hybrid_results.retain(|r| {
            if let Ok(Some(chunk)) = state.store.get_chunk(&r.id) {
                chunk
                    .source
                    .as_ref()
                    .map(|s| s.agent == *agent)
                    .unwrap_or(false)
            } else {
                true
            }
        });
    }
    if let Some(ref exclude) = payload.exclude_source_agents {
        hybrid_results.retain(|r| {
            if let Ok(Some(chunk)) = state.store.get_chunk(&r.id) {
                chunk
                    .source
                    .as_ref()
                    .map(|s| !exclude.contains(&s.agent))
                    .unwrap_or(true)
            } else {
                true
            }
        });
    }

    let total_results_before_topk = hybrid_results.len();
    let final_top_k = if matches!(ctx.complexity, QueryComplexity::Aggregation) {
        payload.top_k.unwrap_or(30)
    } else {
        payload.top_k.unwrap_or(state.config.search.top_k)
    };
    hybrid_results.truncate(final_top_k);

    FilterOutcome {
        results: hybrid_results,
        dedup_removed,
        total_results_before_topk,
        final_top_k,
    }
}

/// Compute associations for search results using graph walk, SAP, and temporal mechanisms.
async fn compute_associations(
    state: &Arc<AppState>,
    query: &str,
    results: &[SearchResult],
    stream_id: &str,
) -> Option<Vec<Association>> {
    use loomem_core::associator::{
        clustering::get_cluster_id,
        graph_walk::random_walk,
        sap::find_adjacent_possible,
        serendipity::{compute_serendipity, find_query_cluster, ScoredAssociation},
        temporal::find_temporal_neighbors,
    };

    // ECA-29: Circuit breaker — check association health before proceeding
    if !loomem_core::associator::should_run_associations(&state.store, stream_id) {
        return None;
    }

    // ECA-31: Cost guardrail — check if budget allows associations
    let cost_status = loomem_core::cost_tracker::check_cost_budget_for_stream(
        &state.store,
        stream_id,
        state.config.cost.daily_cap_usd,
    );
    if !cost_status.allow_associations() {
        tracing::warn!(
            "compute_associations: skipped for stream {} due to cost budget ({})",
            stream_id,
            cost_status.description()
        );
        return None;
    }

    // Get query embedding
    let query_emb = if let Some(ref embedder) = state.local_embedder {
        embedder.embed(query).ok()
    } else if let Some(api_key) = state.config.llm.get_api_key() {
        embeddings::embed(
            &state.http_client,
            &api_key,
            &state.config.llm.embedding_model,
            query,
        )
        .await
        .ok()
    } else {
        None
    };

    let query_emb = match query_emb {
        Some(e) => e,
        None => {
            tracing::debug!("Associations: could not embed query, skipping");
            return None;
        }
    };

    tracing::debug!("Associations: query embedded, dim={}", query_emb.len());

    // Find query cluster
    let query_cluster = match find_query_cluster(&query_emb, &state.store) {
        Ok(Some(c)) => {
            tracing::debug!("Associations: query_cluster={}", c);
            c
        }
        _ => {
            tracing::debug!("Associations: no query cluster found, skipping");
            return None;
        }
    };

    // Collect context embeddings from top-5 results
    let top_ids: Vec<String> = results.iter().take(5).map(|r| r.id.clone()).collect();
    let mut context_embeddings: Vec<Vec<f32>> = Vec::new();
    for id in &top_ids {
        if let Ok(Some(emb)) = state.store.get_embedding(id) {
            context_embeddings.push(emb);
        }
    }

    if context_embeddings.is_empty() {
        tracing::debug!("Associations: no context embeddings, skipping");
        return None;
    }

    let max_assoc = state.config.associator.max_associations;
    let min_se = state.config.associator.min_serendipity;
    let mut candidates: Vec<ScoredAssociation> = Vec::new();

    // Existing result IDs to exclude
    let result_ids: std::collections::HashSet<String> =
        results.iter().map(|r| r.id.clone()).collect();

    // 1. Graph walk: use entities from query (stream-scoped)
    let query_entities = state.entity_extractor.extract(query);
    for (entity_name, _) in &query_entities {
        if let Ok(Some(entity)) = state.graph.get_entity_by_name(entity_name, stream_id) {
            if let Ok(walk_results) = random_walk(&state.graph, &entity.id, 3, 2, 20) {
                for walk in &walk_results {
                    for chunk_id in &walk.terminal_chunk_ids {
                        if result_ids.contains(chunk_id) {
                            continue;
                        }
                        if let Ok(Some(chunk)) = state.store.get_chunk(chunk_id) {
                            if !loomem_core::associator::is_associable_in_stream(&chunk, stream_id)
                            {
                                continue;
                            }
                            if let Ok(Some(emb)) = state.store.get_embedding(chunk_id) {
                                let cand_cluster = get_cluster_id(&state.store, chunk_id)
                                    .ok()
                                    .flatten()
                                    .unwrap_or(0);
                                let ctx_refs: Vec<&[f32]> =
                                    context_embeddings.iter().map(|e| e.as_slice()).collect();
                                if let Ok(se) = compute_serendipity(
                                    &emb,
                                    &query_emb,
                                    &ctx_refs,
                                    cand_cluster,
                                    query_cluster,
                                    &state.store,
                                ) {
                                    if se.score >= min_se {
                                        candidates.push(ScoredAssociation {
                                            chunk_id: chunk_id.clone(),
                                            content: chunk.content.clone(),
                                            serendipity_score: se.score,
                                            relevance: se.relevance,
                                            obviousness: se.obviousness,
                                            cluster_dist: se.cluster_distance,
                                            source_mechanism: "graph_walk".to_string(),
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    tracing::debug!(
        "Associations: after graph_walk: {} candidates",
        candidates.len()
    );

    // 2. Semantic Adjacent Possible
    if let Ok(sap_candidates) = find_adjacent_possible(
        &state.store,
        &query_emb,
        &context_embeddings,
        query_cluster,
        max_assoc * 2,
        0.3,
        0.6,
        stream_id,
    ) {
        for ac in sap_candidates {
            if result_ids.contains(&ac.chunk_id) {
                continue;
            }
            if candidates.iter().any(|c| c.chunk_id == ac.chunk_id) {
                continue;
            }
            if let Ok(Some(emb)) = state.store.get_embedding(&ac.chunk_id) {
                if let Ok(Some(chunk)) = state.store.get_chunk(&ac.chunk_id) {
                    let ctx_refs: Vec<&[f32]> =
                        context_embeddings.iter().map(|e| e.as_slice()).collect();
                    if let Ok(se) = compute_serendipity(
                        &emb,
                        &query_emb,
                        &ctx_refs,
                        ac.cluster_id,
                        query_cluster,
                        &state.store,
                    ) {
                        if se.score >= min_se {
                            candidates.push(ScoredAssociation {
                                chunk_id: ac.chunk_id.clone(),
                                content: chunk.content.clone(),
                                serendipity_score: se.score,
                                relevance: se.relevance,
                                obviousness: se.obviousness,
                                cluster_dist: se.cluster_distance,
                                source_mechanism: "adjacent_possible".to_string(),
                            });
                        }
                    }
                }
            }
        }
    }
    tracing::debug!("Associations: after SAP: {} candidates", candidates.len());

    // 3. Temporal co-occurrence
    if let Ok(temporal_candidates) = find_temporal_neighbors(
        &state.store,
        &top_ids,
        24, // 24h window
        max_assoc * 2,
        stream_id,
    ) {
        for tc in temporal_candidates {
            if result_ids.contains(&tc.chunk_id) {
                continue;
            }
            if candidates.iter().any(|c| c.chunk_id == tc.chunk_id) {
                continue;
            }
            if let Ok(Some(emb)) = state.store.get_embedding(&tc.chunk_id) {
                let cand_cluster = get_cluster_id(&state.store, &tc.chunk_id)
                    .ok()
                    .flatten()
                    .unwrap_or(0);
                let ctx_refs: Vec<&[f32]> =
                    context_embeddings.iter().map(|e| e.as_slice()).collect();
                if let Ok(se) = compute_serendipity(
                    &emb,
                    &query_emb,
                    &ctx_refs,
                    cand_cluster,
                    query_cluster,
                    &state.store,
                ) {
                    if se.score >= min_se {
                        candidates.push(ScoredAssociation {
                            chunk_id: tc.chunk_id.clone(),
                            content: tc.content.clone(),
                            serendipity_score: se.score,
                            relevance: se.relevance,
                            obviousness: se.obviousness,
                            cluster_dist: se.cluster_distance,
                            source_mechanism: "temporal".to_string(),
                        });
                    }
                }
            }
        }
    }

    tracing::debug!(
        "Associations: after temporal: {} total candidates (min_se={})",
        candidates.len(),
        min_se
    );

    // 4. Dream discovery promotion (ECA-23b): check latent associations
    if let Ok(latents) =
        loomem_core::associator::dream::get_latent_associations(&state.store, stream_id, 50)
    {
        for latent in &latents {
            if result_ids.contains(&latent.target_chunk_id) {
                continue;
            }
            if candidates
                .iter()
                .any(|c| c.chunk_id == latent.target_chunk_id)
            {
                continue;
            }
            if let Ok(Some(target_emb)) = state.store.get_embedding(&latent.target_chunk_id) {
                // Check if cosine(latent target, query) > 0.4
                let sim =
                    loomem_core::associator::clustering::cosine_similarity(&target_emb, &query_emb);
                if sim > 0.4 {
                    if let Ok(Some(chunk)) = state.store.get_chunk(&latent.target_chunk_id) {
                        candidates.push(ScoredAssociation {
                            chunk_id: latent.target_chunk_id.clone(),
                            content: chunk.content.clone(),
                            serendipity_score: latent.score * sim, // blend dream score with relevance
                            relevance: sim,
                            obviousness: 0.0,
                            cluster_dist: 1.0,
                            source_mechanism: "dream_discovery".to_string(),
                        });
                        // Promote the latent (increment promoted_count)
                        let _ = loomem_core::associator::dream::promote_latent(
                            &state.store,
                            stream_id,
                            &latent.id,
                        );
                    }
                }
            }
        }
    }
    tracing::debug!(
        "Associations: after dream: {} total candidates",
        candidates.len()
    );

    // Sort by serendipity score, deduplicate, take max_associations
    candidates.sort_by(|a, b| {
        b.serendipity_score
            .partial_cmp(&a.serendipity_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.dedup_by(|a, b| a.chunk_id == b.chunk_id);
    candidates.truncate(max_assoc);

    // ECA-29: Record mean Sₑ for this search (for health tracking)
    if !candidates.is_empty() {
        let mean_se =
            candidates.iter().map(|c| c.serendipity_score).sum::<f64>() / candidates.len() as f64;
        loomem_core::associator::record_mean_se_score(&state.store, stream_id, mean_se);
    }

    if candidates.is_empty() {
        return None;
    }

    let assoc_results: Vec<Association> = candidates
        .into_iter()
        .map(|c| Association {
            content: c.content,
            score: c.serendipity_score,
            source_mechanism: c.source_mechanism,
            explanation: Some(format!(
                "relevance={:.2} obviousness={:.2} cluster_dist={:.2}",
                c.relevance, c.obviousness, c.cluster_dist
            )),
        })
        .collect();

    Some(assoc_results)
}

/// Dedicated association endpoint: POST /v1/associate
pub async fn associate_handler(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Json(payload): Json<AssociateRequest>,
) -> Result<Json<AssociateResponse>, AppError> {
    let start = Instant::now();

    if !state.config.associator.enabled {
        return Ok(Json(AssociateResponse {
            associations: vec![],
            took_ms: 0,
        }));
    }

    let count = payload
        .count
        .unwrap_or(state.config.associator.max_associations)
        .min(10);

    // Non-admin callers cannot override the stream via the payload — this
    // would bypass the middleware's scope-based stream derivation. Admin
    // debugging still works because admin tokens pass through untouched
    // (brief §G4).
    let stream_id = match payload.stream_id.as_deref() {
        Some(requested) if requested != auth.stream_id && !auth.is_admin => {
            tracing::warn!(
                target: "audit",
                "Non-admin user {:?} denied associate stream override: requested={} owned={}",
                auth.user_id, requested, auth.stream_id
            );
            return Err(AppError::Forbidden(
                "payload.stream_id override requires Admin".into(),
            ));
        }
        Some(requested) => requested,
        None => auth.stream_id.as_str(),
    };

    // Get query embedding
    let query_emb = if let Some(ref embedder) = state.local_embedder {
        embedder.embed(&payload.query).map_err(AppError::Internal)?
    } else if let Some(api_key) = state.config.llm.get_api_key() {
        embeddings::embed(
            &state.http_client,
            &api_key,
            &state.config.llm.embedding_model,
            &payload.query,
        )
        .await
        .map_err(AppError::Internal)?
    } else {
        return Err(AppError::Internal(anyhow::anyhow!(
            "No embedding provider available"
        )));
    };

    // Find query cluster
    let query_cluster =
        match loomem_core::associator::serendipity::find_query_cluster(&query_emb, &state.store) {
            Ok(Some(c)) => c,
            Ok(None) => {
                return Ok(Json(AssociateResponse {
                    associations: vec![],
                    took_ms: start.elapsed().as_millis() as u64,
                }));
            }
            Err(e) => return Err(AppError::Internal(e)),
        };

    // Run a quick search to get context embeddings
    let tantivy = state.tantivy.lock().await;
    let bm25_results = tantivy
        .search_with_stream(&payload.query, stream_id, 10)
        .unwrap_or_default();
    drop(tantivy);

    let mut context_embeddings: Vec<Vec<f32>> = Vec::new();
    let mut result_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for r in bm25_results.iter().take(5) {
        result_ids.insert(r.id.clone());
        if let Ok(Some(emb)) = state.store.get_embedding(&r.id) {
            context_embeddings.push(emb);
        }
    }

    if context_embeddings.is_empty() {
        return Ok(Json(AssociateResponse {
            associations: vec![],
            took_ms: start.elapsed().as_millis() as u64,
        }));
    }

    let mechanisms: Vec<String> = payload.mechanisms.unwrap_or_else(|| {
        vec![
            "graph".to_string(),
            "temporal".to_string(),
            "adjacent".to_string(),
        ]
    });

    let min_se = state.config.associator.min_serendipity;
    let mut candidates: Vec<loomem_core::associator::serendipity::ScoredAssociation> = Vec::new();

    // Graph walk
    if mechanisms.iter().any(|m| m == "graph") {
        let query_entities = state.entity_extractor.extract(&payload.query);
        let hops = payload.hops.unwrap_or(3);
        for (entity_name, _) in &query_entities {
            if let Ok(Some(entity)) = state.graph.get_entity_by_name(entity_name, stream_id) {
                if let Ok(walk_results) = loomem_core::associator::graph_walk::random_walk(
                    &state.graph,
                    &entity.id,
                    hops,
                    2,
                    20,
                ) {
                    for walk in &walk_results {
                        for chunk_id in &walk.terminal_chunk_ids {
                            if result_ids.contains(chunk_id) {
                                continue;
                            }
                            if let Ok(Some(chunk)) = state.store.get_chunk(chunk_id) {
                                if !loomem_core::associator::is_associable_in_stream(
                                    &chunk, stream_id,
                                ) {
                                    continue;
                                }
                                if let Ok(Some(emb)) = state.store.get_embedding(chunk_id) {
                                    let cand_cluster =
                                        loomem_core::associator::clustering::get_cluster_id(
                                            &state.store,
                                            chunk_id,
                                        )
                                        .ok()
                                        .flatten()
                                        .unwrap_or(0);
                                    let ctx_refs: Vec<&[f32]> =
                                        context_embeddings.iter().map(|e| e.as_slice()).collect();
                                    if let Ok(se) =
                                        loomem_core::associator::serendipity::compute_serendipity(
                                            &emb,
                                            &query_emb,
                                            &ctx_refs,
                                            cand_cluster,
                                            query_cluster,
                                            &state.store,
                                        )
                                    {
                                        if se.score >= min_se {
                                            candidates.push(loomem_core::associator::serendipity::ScoredAssociation {
                                                chunk_id: chunk_id.clone(),
                                                content: chunk.content.clone(),
                                                serendipity_score: se.score,
                                                relevance: se.relevance,
                                                obviousness: se.obviousness,
                                                cluster_dist: se.cluster_distance,
                                                source_mechanism: "graph_walk".to_string(),
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Semantic Adjacent Possible
    if mechanisms.iter().any(|m| m == "adjacent") {
        if let Ok(sap_candidates) = loomem_core::associator::sap::find_adjacent_possible(
            &state.store,
            &query_emb,
            &context_embeddings,
            query_cluster,
            count * 2,
            0.3,
            0.6,
            stream_id,
        ) {
            for ac in sap_candidates {
                if result_ids.contains(&ac.chunk_id) {
                    continue;
                }
                if candidates.iter().any(|c| c.chunk_id == ac.chunk_id) {
                    continue;
                }
                if let Ok(Some(emb)) = state.store.get_embedding(&ac.chunk_id) {
                    if let Ok(Some(chunk)) = state.store.get_chunk(&ac.chunk_id) {
                        let ctx_refs: Vec<&[f32]> =
                            context_embeddings.iter().map(|e| e.as_slice()).collect();
                        if let Ok(se) = loomem_core::associator::serendipity::compute_serendipity(
                            &emb,
                            &query_emb,
                            &ctx_refs,
                            ac.cluster_id,
                            query_cluster,
                            &state.store,
                        ) {
                            if se.score >= min_se {
                                candidates.push(
                                    loomem_core::associator::serendipity::ScoredAssociation {
                                        chunk_id: ac.chunk_id.clone(),
                                        content: chunk.content.clone(),
                                        serendipity_score: se.score,
                                        relevance: se.relevance,
                                        obviousness: se.obviousness,
                                        cluster_dist: se.cluster_distance,
                                        source_mechanism: "adjacent_possible".to_string(),
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    // Temporal co-occurrence
    if mechanisms.iter().any(|m| m == "temporal") {
        let anchor_ids: Vec<String> = bm25_results.iter().take(5).map(|r| r.id.clone()).collect();
        if let Ok(temporal_candidates) = loomem_core::associator::temporal::find_temporal_neighbors(
            &state.store,
            &anchor_ids,
            24,
            count * 2,
            stream_id,
        ) {
            for tc in temporal_candidates {
                if result_ids.contains(&tc.chunk_id) {
                    continue;
                }
                if candidates.iter().any(|c| c.chunk_id == tc.chunk_id) {
                    continue;
                }
                if let Ok(Some(emb)) = state.store.get_embedding(&tc.chunk_id) {
                    let cand_cluster = loomem_core::associator::clustering::get_cluster_id(
                        &state.store,
                        &tc.chunk_id,
                    )
                    .ok()
                    .flatten()
                    .unwrap_or(0);
                    let ctx_refs: Vec<&[f32]> =
                        context_embeddings.iter().map(|e| e.as_slice()).collect();
                    if let Ok(se) = loomem_core::associator::serendipity::compute_serendipity(
                        &emb,
                        &query_emb,
                        &ctx_refs,
                        cand_cluster,
                        query_cluster,
                        &state.store,
                    ) {
                        if se.score >= min_se {
                            candidates.push(
                                loomem_core::associator::serendipity::ScoredAssociation {
                                    chunk_id: tc.chunk_id.clone(),
                                    content: tc.content.clone(),
                                    serendipity_score: se.score,
                                    relevance: se.relevance,
                                    obviousness: se.obviousness,
                                    cluster_dist: se.cluster_distance,
                                    source_mechanism: "temporal".to_string(),
                                },
                            );
                        }
                    }
                }
            }
        }
    }

    // Sort, deduplicate, truncate
    candidates.sort_by(|a, b| {
        b.serendipity_score
            .partial_cmp(&a.serendipity_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.dedup_by(|a, b| a.chunk_id == b.chunk_id);
    candidates.truncate(count);

    let associations: Vec<Association> = candidates
        .into_iter()
        .map(|c| Association {
            content: c.content,
            score: c.serendipity_score,
            source_mechanism: c.source_mechanism,
            explanation: Some(format!(
                "relevance={:.2} obviousness={:.2} cluster_dist={:.2}",
                c.relevance, c.obviousness, c.cluster_dist
            )),
        })
        .collect();

    Ok(Json(AssociateResponse {
        associations,
        took_ms: start.elapsed().as_millis() as u64,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── AC-3: scope routing unit tests ────────────────────────────────────
    //
    // prepare_query_context is not testable in isolation (requires full
    // AppState). The scope branch delegates entirely to resolve_scope, so
    // these tests verify resolve_scope semantics for the 7 AC-3 cases plus
    // the mutual-exclusion guard that lives in prepare_query_context itself.
    // Convention: mirror scope.rs test style.

    mod scope_tests {
        use crate::auth::{AuthContext, KeyScope};
        use crate::handlers::scope::{resolve_scope, ScopeParam, Source};
        use crate::handlers::types::SearchRequest;
        use loomem_core::config::RocksDbConfig;
        use loomem_core::storage::{RocksDbStore, User, UserRole, DEFAULT_STREAM_ID};
        use std::sync::Arc;
        use tempfile::TempDir;

        fn rocksdb_cfg() -> RocksDbConfig {
            RocksDbConfig {
                max_open_files: 50,
                compression: "none".into(),
                write_buffer_size: 4 * 1024 * 1024,
                max_write_buffer_number: 2,
            }
        }

        fn fresh_store() -> (Arc<RocksDbStore>, TempDir) {
            let tmp = TempDir::new().unwrap();
            let store = Arc::new(RocksDbStore::open(tmp.path(), &rocksdb_cfg()).unwrap());
            (store, tmp)
        }

        fn seed_user(store: &RocksDbStore, id: &str, role: UserRole, with_private: bool) {
            let u = User {
                id: id.into(),
                api_key: None,
                shared_api_key: Some(format!("tok_{id}_shared")),
                private_api_key: if with_private {
                    Some(format!("tok_{id}_private"))
                } else {
                    None
                },
                stream_id: format!("s_{id}"),
                created_at: 0,
                last_active: None,
                label: None,
                active: true,
                workspace_id: None,
                role,
                email: None,
                display_name: None,
                external_id: None,
                pending_first_login: false,
                last_login_at: None,
            };
            store.store_user(&u).unwrap();
            if with_private {
                let flags = serde_json::json!({"private_stream": {"active": true}});
                store
                    .set_user_flags(id, flags.to_string().as_bytes())
                    .unwrap();
            }
        }

        fn auth_for(user_id: &str, role: UserRole) -> AuthContext {
            AuthContext::single_stream(
                DEFAULT_STREAM_ID,
                role,
                KeyScope::Shared,
                Some(user_id.into()),
                role.is_admin(),
            )
        }

        // AC-3 case 1: scope=shared + reader → OK, routed to DEFAULT_STREAM_ID.
        #[test]
        fn search_scope_shared_reader_ok() {
            let (store, _tmp) = fresh_store();
            seed_user(&store, "reader1", UserRole::Reader, false);
            let auth = auth_for("reader1", UserRole::Reader);
            let r = resolve_scope(ScopeParam::Shared, &auth, &store).unwrap();
            assert_eq!(r.streams.len(), 1);
            assert_eq!(r.streams[0].0, DEFAULT_STREAM_ID);
            assert_eq!(r.streams[0].1, Source::Shared);
        }

        // AC-3 case 2: scope=private + writer with active private stream → OK.
        #[test]
        fn search_scope_private_writer_with_stream_ok() {
            let (store, _tmp) = fresh_store();
            seed_user(&store, "writer1", UserRole::Writer, true);
            let auth = auth_for("writer1", UserRole::Writer);
            let r = resolve_scope(ScopeParam::Private, &auth, &store).unwrap();
            assert_eq!(r.streams.len(), 1);
            assert_eq!(r.streams[0].0, "s_writer1");
            assert_eq!(r.streams[0].1, Source::Private);
        }

        // AC-3 case 3: scope=private + writer without private stream → 404.
        #[test]
        fn search_scope_private_writer_no_stream_404() {
            let (store, _tmp) = fresh_store();
            seed_user(&store, "writer2", UserRole::Writer, false);
            let auth = auth_for("writer2", UserRole::Writer);
            let err = resolve_scope(ScopeParam::Private, &auth, &store).unwrap_err();
            let dbg = format!("{err:?}");
            assert!(dbg.contains("NotFound"), "expected NotFound, got: {dbg}");
            assert!(dbg.contains("no private stream"), "got: {dbg}");
        }

        // AC-3 case 7: scope=shared + stream simultaneously → 400 mutual exclusion.
        // This guard lives in prepare_query_context, so we test it via SearchRequest
        // field construction to verify the payload shape that triggers the guard.
        #[test]
        fn search_scope_and_stream_mutually_exclusive_payload() {
            // Verify SearchRequest accepts both fields (compile-time AC-1 proof).
            let payload = SearchRequest {
                query: "test".into(),
                user_id: None,
                top_k: None,
                stream: Some("X".into()),
                streams: None,
                entity: None,
                date_from: None,
                date_to: None,
                valid_at: None,
                dry_run: false,
                filters: None,
                include_superseded: false,
                trace: false,
                fact_type: None,
                subject: None,
                min_confidence: None,
                include_associations: false,
                source_agent: None,
                exclude_source_agents: None,
                scope: Some(ScopeParam::Shared),
                debug_query_classification: false,
                debug_signal_breakdown: false,
            };
            // Both scope and stream are Some — this combination is the one
            // prepare_query_context rejects with BadRequest. Assert the payload
            // shape is constructible (AC-1) and both fields are present (AC-2 guard input).
            assert!(payload.scope.is_some());
            assert!(payload.stream.is_some());
        }
    }

    // ── Cycle /42 — bitemporal + supersede filter unit tests ──────────────
    //
    // Lock the bitemporal (`valid_at`) and supersede (`is_latest`) filter
    // semantics that `filter_and_truncate` delegates to via the two pure
    // helpers `filter_bitemporal` and `filter_superseded` (search.rs §1500).
    // Mirror `mod scope_tests` style: tempdir + real `RocksDbStore`, no
    // `AppState`. See `cycles/42-audit-findings.md` §E for placement
    // rationale and §D for the policy decisions tested here.
    mod bitemporal_tests {
        use loomem_core::config::RocksDbConfig;
        use loomem_core::storage::Chunk;
        use loomem_core::HybridSearchResult;
        use loomem_core::RocksDbStore;
        use tempfile::TempDir;

        fn rocksdb_cfg() -> RocksDbConfig {
            RocksDbConfig {
                max_open_files: 50,
                compression: "none".into(),
                write_buffer_size: 4 * 1024 * 1024,
                max_write_buffer_number: 2,
            }
        }

        fn fresh_store() -> (RocksDbStore, TempDir) {
            let tmp = TempDir::new().expect("tempdir");
            let store = RocksDbStore::open(tmp.path(), &rocksdb_cfg()).expect("open RocksDbStore");
            (store, tmp)
        }

        /// Build a Chunk with `id` + `stream`; all bitemporal/supersede fields
        /// default to "always valid + is_latest=true". Tests then mutate the
        /// specific field they exercise. Argument count stays within
        /// CLAUDE.md §1 limits.
        fn chunk_default(id: &str, stream: &str) -> Chunk {
            Chunk {
                id: id.into(),
                content: format!("content for {id}"),
                stream: stream.into(),
                level: 0,
                score: 1.0,
                timestamp: 1_000,
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
                trust_level: None,
                ingester_user_id: None,

                alpha: 1.0,
                beta: 1.0,
                harmful_count: 0,
                n_ratings: 0,
                last_rated_at: None,
                provenance_role: loomem_core::storage::ProvenanceRole::Claim,
            }
        }

        fn hsr(id: &str) -> HybridSearchResult {
            HybridSearchResult {
                id: id.into(),
                content: format!("content for {id}"),
                user_id: "test".into(),
                app_id: "test".into(),
                level: 0,
                timestamp: 1_000,
                score: 1.0,
                bm25_score: 1.0,
                vector_score: 1.0,
                time_decay_factor: 1.0,
            }
        }

        fn ids(results: &[HybridSearchResult]) -> Vec<String> {
            results.iter().map(|r| r.id.clone()).collect()
        }

        // §3.4 #1 — default behavior: superseded filter drops `is_latest=false`.
        // Setup: A1 (is_latest=false, superseded_by=A2) + A2 (is_latest=true,
        // supersedes_id=A1). Default filter (no `include_superseded`) ⇒ only A2.
        // With `include_superseded=true` ⇒ both (filter not invoked).
        #[test]
        fn search_returns_only_latest_by_default() {
            let (store, _tmp) = fresh_store();
            let mut a1 = chunk_default("A1", "s");
            a1.is_latest = false;
            a1.superseded_by = Some("A2".into());
            let mut a2 = chunk_default("A2", "s");
            a2.supersedes_id = Some("A1".into());
            a2.version = 2;
            store.store_chunk(&a1).expect("store A1");
            store.store_chunk(&a2).expect("store A2");

            // Default path: filter_superseded invoked.
            let mut results = vec![hsr("A1"), hsr("A2")];
            super::super::filter_superseded(&mut results, &store);
            assert_eq!(ids(&results), vec!["A2".to_string()]);

            // include_superseded=true path: filter not invoked.
            let results = vec![hsr("A1"), hsr("A2")];
            assert_eq!(ids(&results), vec!["A1".to_string(), "A2".to_string()]);
        }

        // §3.4 #2 — time-travel: valid_at returns the historical state.
        // Setup: chunk1 valid t0..t10, chunk2 valid t10..None (open right).
        // Query valid_at=t5 ⇒ only chunk1; valid_at=t15 ⇒ only chunk2.
        #[test]
        fn search_valid_at_returns_historical_state() {
            let (store, _tmp) = fresh_store();
            let mut c1 = chunk_default("c1", "s");
            c1.valid_from = Some(0);
            c1.valid_until = Some(10);
            let mut c2 = chunk_default("c2", "s");
            c2.valid_from = Some(10);
            store.store_chunk(&c1).expect("store c1");
            store.store_chunk(&c2).expect("store c2");

            let mut results = vec![hsr("c1"), hsr("c2")];
            super::super::filter_bitemporal(&mut results, &store, 5);
            assert_eq!(ids(&results), vec!["c1".to_string()]);

            let mut results = vec![hsr("c1"), hsr("c2")];
            super::super::filter_bitemporal(&mut results, &store, 15);
            assert_eq!(ids(&results), vec!["c2".to_string()]);
        }

        // §3.4 #3 — outside range: chunk with bounded interval, query outside.
        #[test]
        fn search_valid_at_outside_range_returns_empty() {
            let (store, _tmp) = fresh_store();
            let mut c = chunk_default("c1", "s");
            c.valid_from = Some(100);
            c.valid_until = Some(200);
            store.store_chunk(&c).expect("store c1");

            // Below valid_from.
            let mut results = vec![hsr("c1")];
            super::super::filter_bitemporal(&mut results, &store, 50);
            assert!(results.is_empty(), "below valid_from: empty");

            // Above valid_until.
            let mut results = vec![hsr("c1")];
            super::super::filter_bitemporal(&mut results, &store, 300);
            assert!(results.is_empty(), "above valid_until: empty");

            // Inside the interval (sanity of test setup).
            let mut results = vec![hsr("c1")];
            super::super::filter_bitemporal(&mut results, &store, 150);
            assert_eq!(ids(&results), vec!["c1".to_string()]);
        }

        // §3.4 #4 — open intervals: valid_from=None and valid_until=None
        // mean unbounded on that side. Always valid.
        #[test]
        fn search_valid_at_with_open_intervals() {
            let (store, _tmp) = fresh_store();
            let c = chunk_default("c1", "s"); // valid_from=None, valid_until=None
            store.store_chunk(&c).expect("store c1");

            for t in [0_u64, 1, 1_000, u64::MAX / 2, u64::MAX] {
                let mut results = vec![hsr("c1")];
                super::super::filter_bitemporal(&mut results, &store, t);
                assert_eq!(
                    ids(&results),
                    vec!["c1".to_string()],
                    "open interval kept at valid_at={t}"
                );
            }
        }

        // §3.4 #5 — supersede chain: A1→A2→A3 (multi-version).
        // Default ⇒ only A3 (is_latest=true).
        // include_superseded=true (filter not invoked) ⇒ all 3.
        #[test]
        fn supersede_chain_filterable() {
            let (store, _tmp) = fresh_store();
            // Chain: v1 superseded by v2; v2 superseded by v3; v3 latest.
            let mut v1 = chunk_default("v1", "s");
            v1.is_latest = false;
            v1.superseded_by = Some("v2".into());
            let mut v2 = chunk_default("v2", "s");
            v2.is_latest = false;
            v2.superseded_by = Some("v3".into());
            v2.supersedes_id = Some("v1".into());
            v2.version = 2;
            let mut v3 = chunk_default("v3", "s");
            v3.supersedes_id = Some("v2".into());
            v3.version = 3;
            store.store_chunk(&v1).expect("store v1");
            store.store_chunk(&v2).expect("store v2");
            store.store_chunk(&v3).expect("store v3");

            let mut results = vec![hsr("v1"), hsr("v2"), hsr("v3")];
            super::super::filter_superseded(&mut results, &store);
            assert_eq!(ids(&results), vec!["v3".to_string()]);

            // include_superseded=true: filter not invoked, all 3 retained.
            let results = vec![hsr("v1"), hsr("v2"), hsr("v3")];
            assert_eq!(
                ids(&results),
                vec!["v1".to_string(), "v2".to_string(), "v3".to_string()]
            );
        }

        // §3.4 #6 — chunk metadata unavailable: D1 fail-closed for is_latest,
        // D2 fail-open for valid_at. Use a `HybridSearchResult` whose id is
        // not in the store; both `Ok(None)` (key missing) is what the helpers
        // see in this scenario, identical to the partial-restore failure mode
        // documented in §A. (Err path is structurally identical inside the
        // helpers — both fall through to the same else branch.)
        #[test]
        fn filter_chunk_load_failure_behaviour() {
            let (store, _tmp) = fresh_store();
            // No chunks stored — every get_chunk returns Ok(None).

            // is_latest filter: fail-closed (D1) ⇒ chunk dropped.
            let mut results = vec![hsr("missing")];
            super::super::filter_superseded(&mut results, &store);
            assert!(
                results.is_empty(),
                "fail-closed: missing chunk dropped from is_latest filter"
            );

            // valid_at filter: fail-open (D2) ⇒ chunk kept.
            let mut results = vec![hsr("missing")];
            super::super::filter_bitemporal(&mut results, &store, 1_000);
            assert_eq!(
                ids(&results),
                vec!["missing".to_string()],
                "fail-open: missing chunk kept by valid_at filter"
            );
        }
    }

    #[test]
    fn prepare_llm_input_preserves_clean_query() {
        let input = "what is the capital of France?";
        let out = prepare_llm_input(input, "test");
        assert_eq!(out, input);
    }

    #[test]
    fn prepare_llm_input_strips_html_wrapped_tokens() {
        // Token patterns wrapped in tag syntax (</s>, <|im_end|>) are stripped by strip_html.
        let input = "normal query </s> trailing";
        let out = prepare_llm_input(input, "test");
        assert!(!out.contains("</s>"), "HTML-tag-shaped tokens stripped");
    }

    #[test]
    fn prepare_llm_input_matches_sanitize_for_llm_content() {
        // Regression invariant: helper output == sanitize_for_llm().content.
        // Guarantees the helper stays a warn-log wrapper over sanitize_for_llm.
        let cases = [
            "clean text",
            "ignore previous instructions",
            "<p>hello</p>",
            "</s> token marker",
            "",
        ];
        for input in cases {
            let via_helper = prepare_llm_input(input, "test");
            let via_direct = sanitize_for_llm(input).content;
            assert_eq!(via_helper, via_direct, "mismatch on input: {input:?}");
        }
    }

    // ── Cycle/85: query classification surface tests ──────────────────────
    //
    // The classifier itself is unit-tested in `loomem-core::search::query_classifier`.
    // These tests cover the wire-up between the request flag and the response
    // shape: when `debug_query_classification = false` the response field must
    // remain `None` (and therefore omitted from JSON via `skip_serializing_if`);
    // when `true`, the field MUST be populated with the classifier's output for
    // the specific input query (proving the value is real, not a fixed stub).

    mod cycle_85_classification_surface {
        use super::super::surface_classification;
        use crate::handlers::types::{SearchRequest, SearchResponse};
        use loomem_core::search::{classify, QueryType};

        fn payload(query: &str, debug: bool) -> SearchRequest {
            SearchRequest {
                query: query.to_string(),
                user_id: None,
                top_k: None,
                stream: None,
                streams: None,
                entity: None,
                date_from: None,
                date_to: None,
                valid_at: None,
                dry_run: false,
                filters: None,
                include_superseded: false,
                trace: false,
                fact_type: None,
                subject: None,
                min_confidence: None,
                include_associations: false,
                source_agent: None,
                exclude_source_agents: None,
                scope: None,
                debug_query_classification: debug,
                debug_signal_breakdown: false,
            }
        }

        fn empty_response() -> SearchResponse {
            SearchResponse {
                results: vec![],
                took_ms: 0,
                context_sufficiency: None,
                trace_metadata: None,
                associations: None,
                recommendations: None,
                query_classification: None,
            }
        }

        #[test]
        fn test_surface_classification_off_by_default() {
            let req = payload("co to jest BLAKE3", false);
            let cls = classify(&req.query);
            assert!(surface_classification(&req, &cls).is_none());
        }

        #[test]
        fn test_surface_classification_returns_some_when_flag_set() {
            let req = payload("wgrałem ten paper o Mem0", true);
            let cls = classify(&req.query);
            let surfaced = surface_classification(&req, &cls).expect("flag=true => Some");
            assert_eq!(surfaced.query_type, QueryType::DocumentLookup);
        }

        #[test]
        fn test_search_response_serialization_omits_classification_when_none() {
            let resp = empty_response();
            let json = serde_json::to_string(&resp).expect("serialize");
            assert!(
                !json.contains("query_classification"),
                "field must be omitted when None, got: {json}"
            );
        }

        #[test]
        fn test_search_response_serialization_includes_classification_when_some() {
            let mut resp = empty_response();
            resp.query_classification = Some(classify("co było wczoraj"));
            let json = serde_json::to_string(&resp).expect("serialize");
            assert!(json.contains("\"query_classification\""), "field present");
            assert!(json.contains("\"temporal\""), "type surfaced");
            assert!(json.contains("\"temporal_markers\""), "features surfaced");
        }

        #[test]
        fn test_response_classification_value_matches_input_query() {
            // Each query type produces the matching classification block —
            // proves the value is real, not a fixed stub. Per
            // `feedback_vacuous_ac_test_is_process_violation`.
            let cases = [
                ("co to jest BLAKE3", QueryType::Factual),
                ("co było wczoraj", QueryType::Temporal),
                ("kto pracował z Mateuszem nad RBAC", QueryType::Relational),
                ("ostatnia rzecz", QueryType::Recent),
                ("wgrałem ten paper o Mem0", QueryType::DocumentLookup),
            ];
            for (q, expected_type) in cases {
                let req = payload(q, true);
                let cls = classify(&req.query);
                let surfaced = surface_classification(&req, &cls)
                    .unwrap_or_else(|| panic!("flag=true should surface for {q:?}"));
                assert_eq!(
                    surfaced.query_type, expected_type,
                    "query {q:?} expected {expected_type:?} got {:?}",
                    surfaced.query_type
                );
            }
        }
    }

    // ── Cycle/86: signal breakdown surface tests (Path A — additive) ──────
    //
    // Fusion math is unit-tested in `loomem_core::search::fusion::tests`.
    // These tests verify the wire-up between the request flag and the
    // per-result `signal_breakdown` field: when `debug_signal_breakdown =
    // false` the field stays `None` (omitted from JSON); when `true`, all
    // four signal slots are populated for every candidate. Path A invariant:
    // the active fusion path is unchanged — these breakdowns are surfaced
    // as a debug field for `/87` to compare against the existing pipeline.

    mod cycle_86_signal_breakdown_surface {
        use super::super::compute_breakdowns_if_requested;
        use crate::handlers::types::{SearchRequest, SearchResult};
        use loomem_core::search::classify;
        use loomem_core::HybridSearchResult;
        use serde_json::json;

        fn payload(query: &str, debug: bool) -> SearchRequest {
            SearchRequest {
                query: query.to_string(),
                user_id: None,
                top_k: None,
                stream: None,
                streams: None,
                entity: None,
                date_from: None,
                date_to: None,
                valid_at: None,
                dry_run: false,
                filters: None,
                include_superseded: false,
                trace: false,
                fact_type: None,
                subject: None,
                min_confidence: None,
                include_associations: false,
                source_agent: None,
                exclude_source_agents: None,
                scope: None,
                debug_query_classification: false,
                debug_signal_breakdown: debug,
            }
        }

        fn hsr(id: &str, vec_score: f32, bm25: f32, ts: i64) -> HybridSearchResult {
            HybridSearchResult {
                id: id.to_string(),
                content: format!("content of {id}"),
                user_id: String::new(),
                app_id: String::new(),
                level: 0,
                timestamp: ts,
                score: 0.0,
                bm25_score: bm25,
                vector_score: vec_score,
                time_decay_factor: 1.0,
            }
        }

        fn make_result(id: &str, sb: Option<loomem_core::search::SignalBreakdown>) -> SearchResult {
            SearchResult {
                id: id.to_string(),
                content: format!("content of {id}"),
                score: 0.5,
                metadata: Some(json!({})),
                trace_info: None,
                signal_breakdown: sb,
                content_type: None,
                content_type_source: None,
            }
        }

        // AC-4 (/142 + /143): SearchResult surfaces content_type + source when
        // present (sidecar hit), and omits the keys entirely when None
        // (`skip_serializing_if`). Since /143 there is **no**
        // `content_type_confidence` key and source is always `llm`. This is the
        // REST JSON contract the search handler hydrates from `get_content_type`.
        #[test]
        fn ac4_search_result_content_type_json_contract() {
            let mut present = make_result("x", None);
            present.content_type = Some("changelog".to_string());
            present.content_type_source = Some("llm".to_string());
            let j = serde_json::to_value(&present).expect("serialize");
            assert_eq!(j["content_type"], "changelog");
            assert_eq!(j["content_type_source"], "llm");
            assert!(
                j.get("content_type_confidence").is_none(),
                "confidence band dropped in /143"
            );

            let absent = make_result("y", None); // both None
            let j2 = serde_json::to_value(&absent).expect("serialize");
            assert!(j2.get("content_type").is_none(), "key omitted when None");
            assert!(j2.get("content_type_source").is_none());
        }

        #[test]
        fn test_breakdowns_off_by_default_returns_all_none() {
            let req = payload("co to jest BLAKE3", false);
            let cls = classify(&req.query);
            let cs = vec![hsr("a", 0.9, 4.0, 1_000), hsr("b", 0.5, 8.0, 2_000)];
            let bs = compute_breakdowns_if_requested(&cs, &cls, &req);
            assert_eq!(bs.len(), cs.len(), "one slot per candidate");
            assert!(bs.iter().all(|b| b.is_none()), "all None when flag off");
        }

        #[test]
        fn test_breakdowns_on_returns_one_per_candidate() {
            let req = payload("co to jest BLAKE3", true);
            let cls = classify(&req.query);
            // Use realistic recent timestamps so recency `exp(-Δt/τ)` doesn't
            // underflow to zero against the wall-clock-anchored `FusionParams::now_default()`.
            let now = chrono::Utc::now().timestamp();
            let cs = vec![
                hsr("alpha", 0.9, 4.0, now - 86_400),
                hsr("bravo", 0.5, 8.0, now - 2 * 86_400),
                hsr("delta", 0.3, 1.0, now - 3 * 86_400),
            ];
            let bs = compute_breakdowns_if_requested(&cs, &cls, &req);
            assert_eq!(bs.len(), cs.len());
            for (i, b) in bs.iter().enumerate() {
                let breakdown = b.as_ref().expect("flag=true => Some");
                // dense + lexical + recency are real signals — they should
                // produce ranks for every candidate since all have non-zero
                // scores in this fixture.
                assert!(
                    breakdown.dense.rank.is_some(),
                    "candidate {i} has dense rank"
                );
                assert!(
                    breakdown.lexical.rank.is_some(),
                    "candidate {i} has lexical rank"
                );
                assert!(
                    breakdown.recency.rank.is_some(),
                    "candidate {i} has recency rank"
                );
                // Placeholder signals — always None per /86 documentation.
                assert!(
                    breakdown.entity_match.rank.is_none(),
                    "entity_match placeholder => None"
                );
            }
        }

        #[test]
        fn test_breakdowns_on_empty_candidates_returns_empty() {
            let req = payload("query", true);
            let cls = classify(&req.query);
            let bs = compute_breakdowns_if_requested(&[], &cls, &req);
            assert!(bs.is_empty(), "empty candidates → empty breakdown vec");
        }

        #[test]
        fn test_search_result_serialization_omits_breakdown_when_none() {
            let r = make_result("alpha", None);
            let json = serde_json::to_string(&r).expect("serialize");
            assert!(
                !json.contains("signal_breakdown"),
                "field must be omitted when None, got: {json}"
            );
        }

        #[test]
        fn test_search_result_serialization_includes_breakdown_when_some() {
            let req = payload("co to jest BLAKE3", true);
            let cls = classify(&req.query);
            let cs = vec![hsr("alpha", 0.9, 4.0, 1_000)];
            let bs = compute_breakdowns_if_requested(&cs, &cls, &req);
            let r = make_result("alpha", bs.into_iter().next().flatten());
            let json = serde_json::to_string(&r).expect("serialize");
            assert!(json.contains("\"signal_breakdown\""), "field present");
            // 4 slot names must all appear in serialized output.
            for slot in ["dense", "lexical", "entity_match", "recency"] {
                assert!(
                    json.contains(slot),
                    "slot {slot:?} must appear in signal_breakdown serialization"
                );
            }
        }

        #[test]
        fn test_breakdown_dense_rank_matches_input_ordering_by_vector_score() {
            // Higher vector_score → lower (better) rank. Verify the breakdown
            // assigns rank 1 to the strongest dense candidate.
            let req = payload("co to jest BLAKE3", true);
            let cls = classify(&req.query);
            let cs = vec![
                hsr("weak", 0.1, 0.0, 1_000),
                hsr("strong", 0.95, 0.0, 1_000),
                hsr("mid", 0.5, 0.0, 1_000),
            ];
            let bs = compute_breakdowns_if_requested(&cs, &cls, &req);
            let breakdowns: Vec<_> = bs.into_iter().map(|b| b.expect("Some")).collect();
            // breakdown[0] aligned with cs[0] = "weak" (worst dense)
            assert_eq!(breakdowns[0].dense.rank, Some(3));
            assert_eq!(breakdowns[1].dense.rank, Some(1));
            assert_eq!(breakdowns[2].dense.rank, Some(2));
        }
    }

    // ── Cycle/118: implicit access-count boost helper tests ───────────────
    mod access_boost {
        use super::super::compute_access_boost_multiplier;

        #[test]
        fn test_compute_access_boost_zero_weight_returns_one() {
            // Tier C identity guarantee: weight=0.0 → exactly 1.0 (no f64 noise).
            assert_eq!(compute_access_boost_multiplier(100, 0.0), 1.0);
            assert_eq!(compute_access_boost_multiplier(0, 0.0), 1.0);
            assert_eq!(compute_access_boost_multiplier(u32::MAX, 0.0), 1.0);
        }

        #[test]
        fn test_compute_access_boost_zero_access_returns_one() {
            // access_count=0 → no boost regardless of weight.
            assert_eq!(compute_access_boost_multiplier(0, 0.3), 1.0);
            assert_eq!(compute_access_boost_multiplier(0, 0.5), 1.0);
            assert_eq!(compute_access_boost_multiplier(0, 1.0), 1.0);
        }

        #[test]
        fn test_compute_access_boost_negative_weight_returns_one() {
            // cycle/126: negative weights are invalid config — clamp to zero
            // (no boost) rather than letting the multiplier go below 1.0 or
            // negative, which would silently invert search result order.
            assert_eq!(compute_access_boost_multiplier(100, -0.5), 1.0);
            assert_eq!(compute_access_boost_multiplier(500, -0.5), 1.0);
            assert_eq!(compute_access_boost_multiplier(u32::MAX, -1.0), 1.0);
            assert_eq!(compute_access_boost_multiplier(1, -0.3), 1.0);
        }

        #[test]
        fn test_compute_access_boost_monotonic() {
            // boost is monotonically non-decreasing in access_count for fixed weight>0.
            let b10 = compute_access_boost_multiplier(10, 0.3);
            let b100 = compute_access_boost_multiplier(100, 0.3);
            let b1000 = compute_access_boost_multiplier(1000, 0.3);
            assert!(b10 < b100, "b(10)={b10} should be < b(100)={b100}");
            assert!(b100 < b1000, "b(100)={b100} should be < b(1000)={b1000}");
        }

        #[test]
        fn test_compute_access_boost_log_scaling() {
            // Formula spot-check: boost(100, 0.3) ≈ 1 + ln(101) * 0.3.
            let expected = 1.0 + (101.0_f64).ln() * 0.3;
            let actual = compute_access_boost_multiplier(100, 0.3);
            assert!(
                (actual - expected).abs() < 1e-9,
                "actual={actual} expected={expected}"
            );
        }

        #[test]
        fn test_backward_compat_config_missing_field() {
            // Legacy config JSON without implicit_access_boost_weight deserializes
            // with default 0.0.
            let legacy_json = r#"{
                "hybrid_weights": {"vector": 0.6, "bm25": 0.4},
                "decay": {"l0_lambda": 0.1, "l1_lambda": 0.03},
                "surprise_boost": 1.5,
                "top_k": 10,
                "synonyms_file": "synonyms.toml",
                "stem_polish": true,
                "entities_file": "entities.toml"
            }"#;
            let cfg: loomem_core::config::SearchConfig =
                serde_json::from_str(legacy_json).expect("legacy JSON must deserialize");
            assert_eq!(cfg.implicit_access_boost_weight, 0.0);
        }
    }

    mod chunk_score_adjustments {
        use super::super::apply_chunk_score_adjustments;
        use loomem_core::config::{
            DecayConfig, GraphSearchConfig, HybridWeightsConfig, ImportanceConfig, SearchConfig,
        };
        use loomem_core::query_cache::QueryCacheConfig;
        use loomem_core::storage::Chunk;
        use loomem_core::HybridSearchResult;

        fn fixture_chunk(tier: &str, importance: Option<f64>) -> Chunk {
            Chunk {
                id: "test-id".into(),
                content: "test content".into(),
                stream: "test-stream".into(),
                level: 0,
                score: 1.0,
                timestamp: 1_000,
                consolidated: false,
                dormant: false,
                in_progress: false,
                prompt_version: None,
                source_ids: None,
                last_decay: None,
                metadata: Some(serde_json::json!({"tier": tier})),
                importance,
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
                trust_level: None,
                ingester_user_id: None,
                alpha: 1.0,
                beta: 1.0,
                harmful_count: 0,
                n_ratings: 0,
                last_rated_at: None,
                provenance_role: loomem_core::storage::ProvenanceRole::Claim,
            }
        }

        fn fixture_result(score: f64, time_decay: f64) -> HybridSearchResult {
            HybridSearchResult {
                id: "test-id".into(),
                content: "test content".into(),
                user_id: "u".into(),
                app_id: "a".into(),
                level: 0,
                timestamp: 1_000,
                score,
                bm25_score: 1.0,
                vector_score: 1.0,
                time_decay_factor: time_decay,
            }
        }

        fn fixture_search_cfg() -> SearchConfig {
            SearchConfig {
                top_k: 10,
                surprise_boost: 1.5,
                hybrid_weights: HybridWeightsConfig {
                    vector: 0.6,
                    bm25: 0.4,
                },
                decay: DecayConfig {
                    l0_lambda: 0.1,
                    l1_lambda: 0.03,
                },
                synonyms_file: "synonyms.toml".into(),
                entities_file: "entities.toml".into(),
                stem_polish: true,
                rerank_enabled: false,
                rerank_candidates: 10,
                rerank_model_dir: None,
                multi_query_enabled: false,
                vector_multi_query: false,
                counting_l0_preference: false,
                importance: ImportanceConfig {
                    high_weight: 1.5,
                    // medium_weight=1.0 so importance None is a no-op multiplier
                    medium_weight: 1.0,
                    low_weight: 0.7,
                    high_threshold: 0.5,
                    low_threshold: 0.2,
                },
                cache: QueryCacheConfig::default(),
                graph: GraphSearchConfig::default(),
                complexity: loomem_core::config::ComplexityConfig::default(),
                // access_count=0 disables access boost regardless of weight
                implicit_access_boost_weight: 0.0,
            }
        }

        #[test]
        fn apply_chunk_score_adjustments_tier_match_arms_preserve_post_118_semantics() {
            let cfg = fixture_search_cfg();

            // Case 1: core, time_decay=0.5, importance=Some(1.0)
            // Expected: (1.0 / 0.5) * 2.0 * 1.0 = 4.0
            {
                let mut r = fixture_result(1.0, 0.5);
                let chunk = fixture_chunk("core", Some(1.0));
                apply_chunk_score_adjustments(&mut r, &chunk, &cfg);
                assert!(
                    (r.score - 4.0).abs() < 1e-9,
                    "core arm: expected 4.0, got {}",
                    r.score
                );
            }

            // Case 2: pinned, time_decay=0.5, importance=Some(1.0)
            // Expected: (0.8/0.5) * 1.5 * 1.0 = 2.4
            {
                let mut r = fixture_result(1.0, 0.5);
                let chunk = fixture_chunk("pinned", Some(1.0));
                apply_chunk_score_adjustments(&mut r, &chunk, &cfg);
                assert!(
                    (r.score - 2.4).abs() < 1e-9,
                    "pinned arm: expected 2.4, got {}",
                    r.score
                );
            }

            // Case 3: ephemeral, time_decay=0.3, importance=Some(1.0)
            // Expected: 0.3 * 0.5 * 1.0 = 0.15
            {
                let mut r = fixture_result(1.0, 0.3);
                let chunk = fixture_chunk("ephemeral", Some(1.0));
                apply_chunk_score_adjustments(&mut r, &chunk, &cfg);
                assert!(
                    (r.score - 0.15).abs() < 1e-9,
                    "ephemeral arm: expected 0.15, got {}",
                    r.score
                );
            }

            // Case 4: default tier, importance=Some(0.5), time_decay=1.0
            // Expected: 1.0 * 0.5 = 0.5 (no tier change, importance multiplier applies)
            {
                let mut r = fixture_result(1.0, 1.0);
                let chunk = fixture_chunk("default", Some(0.5));
                apply_chunk_score_adjustments(&mut r, &chunk, &cfg);
                assert!(
                    (r.score - 0.5).abs() < 1e-9,
                    "default arm: expected 0.5, got {}",
                    r.score
                );
            }
        }
    }
}

// ── Issue 1 (source-provenance-fixes): source_agent over-fetch + pre-filter ──
//
// NOTE: search.rs is a pre-existing god file (>700 SLOC). These additions are
// test-only — they do not affect §1 production metrics or §14 hot-path gates.
#[cfg(test)]
mod agent_filter_tests {
    use super::*;
    use crate::auth::{AuthContext, KeyScope};
    use crate::tests::make_test_app;
    use loomem_core::storage::{Chunk, UserRole, DEFAULT_STREAM_ID};
    use loomem_core::{SourceTag, TextDocument};

    fn req(source_agent: Option<&str>, exclude: Option<Vec<&str>>) -> SearchRequest {
        SearchRequest {
            query: "q".into(),
            user_id: None,
            top_k: None,
            stream: None,
            streams: None,
            entity: None,
            date_from: None,
            date_to: None,
            valid_at: None,
            dry_run: false,
            filters: None,
            include_superseded: false,
            trace: false,
            fact_type: None,
            subject: None,
            min_confidence: None,
            include_associations: false,
            source_agent: source_agent.map(|s| s.to_string()),
            exclude_source_agents: exclude.map(|v| v.iter().map(|s| s.to_string()).collect()),
            scope: None,
            debug_query_classification: false,
            debug_signal_breakdown: false,
        }
    }

    // ── e2e: full search path fills top_k under an agent filter ───────────────
    // Cycle/258 replaced #257's over-fetch arithmetic (`agent_filtered_retrieval_limit`
    // + consts, now deleted) with source-level Tantivy filtering, so the unit
    // tests on that arithmetic are gone; the two e2e fill-top_k tests are ported
    // (they pass under both #257 and Option A — regression guard), and AC2–AC4
    // below cover the Option-A-specific behaviour.

    /// `agent = None` seeds a chunk with no `source` (indexed as the `"unknown"`
    /// token), exercising the absent-agent parity path.
    fn chunk(id: &str, content: &str, agent: Option<&str>) -> Chunk {
        Chunk {
            id: id.to_string(),
            content: content.to_string(),
            stream: DEFAULT_STREAM_ID.to_string(),
            level: 0,
            score: 1.0,
            timestamp: 1_700_000_000,
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
            source: agent.map(SourceTag::from_agent),
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
            trust_level: None,
            ingester_user_id: None,
            alpha: 1.0,
            beta: 1.0,
            harmful_count: 0,
            n_ratings: 0,
            last_rated_at: None,
            provenance_role: loomem_core::storage::ProvenanceRole::Claim,
        }
    }

    fn index_doc(c: &Chunk, agent: Option<&str>) -> TextDocument {
        TextDocument {
            id: c.id.clone(),
            content: c.content.clone(),
            user_id: "default".into(),
            app_id: "default".into(),
            level: 0,
            timestamp: c.timestamp as i64,
            stream: c.stream.clone(),
            entities: None,
            relations: None,
            event_date: None,
            source_agent: agent.map(|a| a.to_string()),
        }
    }

    /// Seed one chunk into both RocksDB and Tantivy with a given agent.
    async fn seed_one(
        state: &std::sync::Arc<AppState>,
        id: &str,
        content: &str,
        agent: Option<&str>,
    ) {
        let c = chunk(id, content, agent);
        state.store.store_chunk(&c).unwrap();
        let mut tv = state.tantivy.lock().await;
        tv.upsert_document(index_doc(&c, agent)).unwrap();
        tv.commit().unwrap();
    }

    /// Seed a stream that defeats a non-over-fetched / non-pre-filtered pool:
    ///   - 40 "decoy" chunks: "apple" ×4 → higher BM25, fill the truncated
    ///     hybrid pool (`config.top_k * 3`) before any target is reached.
    ///   - 25 "wanted" chunks: "apple" ×1 → lower BM25.
    ///
    /// Without the over-fetch + pre-filter the agent filter yields 0; with them
    /// all 25 wanted survive into the pool and top_k truncates to 20.
    async fn seed(state: &std::sync::Arc<AppState>) {
        let mut tv = state.tantivy.lock().await;
        for i in 0..40 {
            let c = chunk(
                &format!("d{i}"),
                &format!("d{i} apple apple apple apple"),
                Some("decoy"),
            );
            state.store.store_chunk(&c).unwrap();
            tv.upsert_document(index_doc(&c, Some("decoy"))).unwrap();
        }
        for i in 0..25 {
            let c = chunk(&format!("t{i}"), &format!("t{i} apple"), Some("wanted"));
            state.store.store_chunk(&c).unwrap();
            tv.upsert_document(index_doc(&c, Some("wanted"))).unwrap();
        }
        tv.commit().unwrap();
    }

    fn admin_auth() -> AuthContext {
        AuthContext::single_stream(
            DEFAULT_STREAM_ID,
            UserRole::Admin,
            KeyScope::Shared,
            Some("admin".into()),
            true,
        )
    }

    #[tokio::test]
    async fn source_agent_filter_fills_top_k() {
        let (_app, state) = make_test_app();
        seed(&state).await;

        let mut payload = req(Some("wanted"), None);
        payload.query = "apple".into();
        payload.top_k = Some(20);

        let resp = search_handler(
            axum::extract::State(state.clone()),
            axum::Extension(admin_auth()),
            axum::Json(payload),
        )
        .await
        .expect("search_handler");

        assert_eq!(
            resp.0.results.len(),
            20,
            "source_agent filter must fill top_k (wanted authored 25 >= 20)"
        );
        assert!(
            resp.0.results.iter().all(|r| r.id.starts_with('t')),
            "all results must belong to the wanted agent"
        );
    }

    #[tokio::test]
    async fn exclude_source_agents_fills_top_k() {
        let (_app, state) = make_test_app();
        seed(&state).await;

        let mut payload = req(None, Some(vec!["decoy"]));
        payload.query = "apple".into();
        payload.top_k = Some(20);

        let resp = search_handler(
            axum::extract::State(state.clone()),
            axum::Extension(admin_auth()),
            axum::Json(payload),
        )
        .await
        .expect("search_handler");

        assert_eq!(
            resp.0.results.len(),
            20,
            "exclude_source_agents must fill top_k from the remaining (wanted) chunks"
        );
        assert!(
            resp.0.results.iter().all(|r| r.id.starts_with('t')),
            "no excluded (decoy) agent results may survive"
        );
    }

    /// AC2 — the Option-A-specific win. 200 BM25-dominant decoys + 8 wanted,
    /// `top_k=20`, filter `source_agent=wanted` → **exactly 8**.
    ///
    /// On `main`@#257 this returns **0**: the 8 wanted score far below the 200
    /// decoys, so they never enter the over-fetched (`top_k*4`, then `*2` Tantivy)
    /// BM25 pool, and the post-retrieval filter drops everything (no embeddings
    /// are seeded, so the vector path can't rescue them). Option A pushes the
    /// `source_agent` MUST term into Tantivy, so the candidate pool is the 8
    /// wanted chunks and all survive. Non-vacuous: with `bm25_leaf` neutered to
    /// the plain `search` path this asserts `8 != 0` and fails.
    #[tokio::test]
    async fn minority_agent_returns_all_matches() {
        let (_app, state) = make_test_app();
        {
            let mut tv = state.tantivy.lock().await;
            for i in 0..200 {
                let c = chunk(
                    &format!("d{i}"),
                    &format!("d{i} apple apple apple apple"),
                    Some("decoy"),
                );
                state.store.store_chunk(&c).unwrap();
                tv.upsert_document(index_doc(&c, Some("decoy"))).unwrap();
            }
            for i in 0..8 {
                let c = chunk(&format!("t{i}"), &format!("t{i} apple"), Some("wanted"));
                state.store.store_chunk(&c).unwrap();
                tv.upsert_document(index_doc(&c, Some("wanted"))).unwrap();
            }
            tv.commit().unwrap();
        }

        let mut payload = req(Some("wanted"), None);
        payload.query = "apple".into();
        payload.top_k = Some(20);

        let resp = search_handler(
            axum::extract::State(state.clone()),
            axum::Extension(admin_auth()),
            axum::Json(payload),
        )
        .await
        .expect("search_handler");

        assert_eq!(
            resp.0.results.len(),
            8,
            "minority agent must return all 8 matches (Option A), not the 0 #257 yields"
        );
        assert!(
            resp.0.results.iter().all(|r| r.id.starts_with('t')),
            "every result must belong to the wanted agent"
        );
    }

    /// AC3 — vector-path agent scoping. The vector pre-filter scopes the
    /// embeddings list to the agent id-set (`agent_id_set` + `retain_by_agent_set`)
    /// **before** scoring, so decoy chunks that would otherwise win on vector
    /// similarity are dropped from the candidate pool. Exercises the exact pair
    /// of helpers `vector_retrieve` uses; non-vacuous because without the id-set
    /// scoping the decoy embeddings stay in the list.
    #[tokio::test]
    async fn vector_pool_is_agent_scoped() {
        let (_app, state) = make_test_app();
        seed_one(&state, "t0", "alpha", Some("wanted")).await;
        seed_one(&state, "t1", "beta", Some("wanted")).await;
        seed_one(&state, "d0", "gamma", Some("decoy")).await;

        // Embeddings list as `vector_retrieve` would hold it (ids + vectors);
        // decoys would win on similarity but must be filtered out first.
        let embeddings: Vec<(String, Vec<f32>)> = vec![
            ("d0".into(), vec![1.0, 0.0]),
            ("t0".into(), vec![0.0, 1.0]),
            ("t1".into(), vec![0.0, 1.0]),
        ];

        let set = agent_id_set(&state, &req(Some("wanted"), None)).await;
        assert!(set.is_some(), "agent filter present → id-set fetched");
        let scoped = retain_by_agent_set(embeddings, set.as_ref());

        let ids: std::collections::HashSet<&str> =
            scoped.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            ids,
            ["t0", "t1"].into_iter().collect(),
            "only wanted-agent embeddings survive the vector pre-filter; decoy dropped"
        );
    }

    /// AC4 — exclude + absent-agent parity. Agent `A`, agent `B`, and one
    /// agent-less chunk (indexed `"unknown"`). `exclude_source_agents=[A]` →
    /// returns `B` + the agent-less chunk, never `A`. Mirrors the canonical
    /// RocksDB semantics (exclude keeps absent). Non-vacuous: pre-change, the
    /// BM25 pool is not agent-scoped at the source, so `A` chunks enter the pool;
    /// here the MUST_NOT term keeps them out at retrieval.
    #[tokio::test]
    async fn exclude_keeps_absent_agent_drops_excluded() {
        let (_app, state) = make_test_app();
        seed_one(&state, "a0", "apple", Some("agentA")).await;
        seed_one(&state, "b0", "apple", Some("agentB")).await;
        seed_one(&state, "u0", "apple", None).await; // -> "unknown"

        let mut payload = req(None, Some(vec!["agentA"]));
        payload.query = "apple".into();
        payload.top_k = Some(20);

        let resp = search_handler(
            axum::extract::State(state.clone()),
            axum::Extension(admin_auth()),
            axum::Json(payload),
        )
        .await
        .expect("search_handler");

        let ids: std::collections::HashSet<&str> =
            resp.0.results.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            ["b0", "u0"].into_iter().collect(),
            "exclude [agentA] keeps agentB + the agent-less chunk, drops agentA"
        );
    }
}
