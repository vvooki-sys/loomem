//! Server side of `[search] state_followup_*` (core logic and limits:
//! `loomem_core::hybrid_search::state_followup`).
//!
//! Runs inside `retrieve_primary` right after the first hybrid fusion and
//! before the latest-state promotion: pick seeds from the requested top-k,
//! embed each seed's content with the same dispatch as the user query
//! (local embedder first, else the OpenAI key; no LLM), rank it against the
//! already stream/date/agent-filtered embedding list of the main query, and
//! merge the hits into the fused pool. Each seed embedding is counted as an
//! extra embedding attempt in the request telemetry.
//!
//! Gate: enabled, and either no date filter on the request
//! (`date_from`/`date_to` or a date parsed from the query) or
//! `state_followup_with_date_filter = true`, and an embedder available
//! (otherwise the whole step is skipped). Not applied to the hosted compat
//! search or the ambient path, which do not go through `retrieve_primary`.
use std::collections::HashSet;

use loomem_core::hybrid_search::state_followup::{FollowupPool, SeedHits};
use loomem_core::hybrid_search::SearchCandidateBudget;
use loomem_core::vector_search::vector_search;
use loomem_core::HybridSearchResult;
use tracing::warn;

use super::super::{telemetry, QueryContext};
use super::{embed_query, VectorInputs};
use crate::handlers::types::StateFollowupDiag;
use crate::AppState;

/// Whether the step runs for this request (see the module docs).
pub(super) fn runs(state: &AppState, ctx: &QueryContext) -> bool {
    let cfg = &state.config.search.state_followup;
    cfg.enabled
        && (cfg.with_date_filter || ctx.date_filter.is_none())
        && (state.local_embedder.is_some() || state.config.llm.get_api_key().is_some())
}

/// Apply the follow-up to the fused `results`. `None` when the step did not
/// run (the pool is untouched); otherwise the diagnostics trail.
pub(super) async fn apply(
    state: &AppState,
    ctx: &QueryContext,
    inputs: &VectorInputs,
    results: &mut Vec<HybridSearchResult>,
    budget: SearchCandidateBudget,
    guaranteed: Option<&HashSet<String>>,
) -> Option<StateFollowupDiag> {
    if !runs(state, ctx) {
        return None;
    }
    let hits_per_seed = state.config.search.state_followup.hits_per_seed;
    let seeds = state
        .hybrid_search
        .state_followup_seeds(results, ctx.limit, &state.store);
    let seed_ids: Vec<String> = seeds.iter().map(|seed| seed.id.clone()).collect();
    let mut seed_hits = Vec::with_capacity(seeds.len());
    for seed in seeds {
        telemetry::record_extra_embedding_attempt(ctx);
        match embed_query(state, &seed.content).await {
            Ok(embedding) => seed_hits.push(SeedHits {
                seed_id: seed.id,
                hits: vector_search(&inputs.embeddings, &embedding, hits_per_seed),
            }),
            Err(e) => warn!(seed = %seed.id, "state_followup: failed to embed seed: {e}"),
        }
    }
    let embedded = seed_hits.len();
    let outcome = state.hybrid_search.merge_state_followup(
        results,
        seed_hits,
        FollowupPool {
            store: &state.store,
            budget,
            guaranteed,
        },
    );
    Some(StateFollowupDiag {
        seeds: seed_ids,
        injected_ids: outcome.injected_ids,
        embedded,
    })
}
