//! Server side of `[search] state_followup_*` (core logic and limits:
//! `loomem_core::hybrid_search::state_followup`).
//!
//! Runs inside `retrieve_primary` right after the first hybrid fusion and
//! before the latest-state promotion: pick seeds from the requested top-k,
//! take each seed's stored vector (from the already stream/date/agent-
//! filtered embedding list of the main query, else from the store), rank it
//! against that filtered list, and merge the hits into the fused pool. Only
//! a seed without a stored vector is embedded afresh, with the same dispatch
//! as the user query (local embedder first, else the OpenAI key; no LLM),
//! counted as an extra embedding attempt in the request telemetry; without
//! an embedder such a seed is skipped.
//!
//! Gate: enabled, and either no date filter on the request
//! (`date_from`/`date_to` or a date parsed from the query) or
//! `state_followup_with_date_filter = true`. Not applied to the hosted
//! compat search or the ambient path, which do not go through
//! `retrieve_primary`.
use std::collections::{HashMap, HashSet};

use loomem_core::hybrid_search::state_followup::{FollowupPool, Seed, SeedHits};
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
    cfg.enabled && (cfg.with_date_filter || ctx.date_filter.is_none())
}

/// Where a seed's query vector came from.
enum SeedVector {
    Stored(Vec<f32>),
    Embedded(Vec<f32>),
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
    if seeds.is_empty() {
        return Some(StateFollowupDiag {
            seeds: seed_ids,
            injected_ids: Vec::new(),
            seeds_from_store: 0,
            embedded: 0,
        });
    }
    let stored: HashMap<&str, &[f32]> = inputs
        .embeddings
        .iter()
        .map(|(id, vector)| (id.as_str(), vector.as_slice()))
        .collect();
    let mut seed_hits = Vec::with_capacity(seeds.len());
    let (mut seeds_from_store, mut embedded) = (0_usize, 0_usize);
    for seed in seeds {
        let vector = match seed_vector(state, ctx, &stored, &seed).await {
            Some(SeedVector::Stored(vector)) => {
                seeds_from_store += 1;
                vector
            }
            Some(SeedVector::Embedded(vector)) => {
                embedded += 1;
                vector
            }
            None => continue,
        };
        seed_hits.push(SeedHits {
            seed_id: seed.id,
            hits: vector_search(&inputs.embeddings, &vector, hits_per_seed),
        });
    }
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
        seeds_from_store,
        embedded,
    })
}

/// The seed's stored vector (filtered list, then store); a fresh embedding
/// of its content only when none is stored and an embedder exists.
async fn seed_vector(
    state: &AppState,
    ctx: &QueryContext,
    stored: &HashMap<&str, &[f32]>,
    seed: &Seed,
) -> Option<SeedVector> {
    if let Some(vector) = stored.get(seed.id.as_str()) {
        return Some(SeedVector::Stored(vector.to_vec()));
    }
    match state.store.get_embedding(&seed.id) {
        Ok(Some(vector)) => return Some(SeedVector::Stored(vector)),
        Ok(None) => {}
        Err(e) => warn!(seed = %seed.id, "state_followup: failed to read seed vector: {e}"),
    }
    if state.local_embedder.is_none() && state.config.llm.get_api_key().is_none() {
        warn!(seed = %seed.id, "state_followup: no stored vector and no embedder; seed skipped");
        return None;
    }
    telemetry::record_extra_embedding_attempt(ctx);
    match embed_query(state, &seed.content).await {
        Ok(vector) => Some(SeedVector::Embedded(vector)),
        Err(e) => {
            warn!(seed = %seed.id, "state_followup: failed to embed seed: {e}");
            None
        }
    }
}
