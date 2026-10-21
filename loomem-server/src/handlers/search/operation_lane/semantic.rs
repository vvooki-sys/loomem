//! Primary-only context selection; no additional retrieval leaves.
use std::sync::Arc;

use loomem_core::cost_tracker::{check_cost_budget, check_cost_budget_for_stream};
use loomem_core::search::operation::OperationPolicy;
use loomem_core::HybridSearchResult;

use super::super::QueryContext;
use super::LaneOutcome;
use crate::handlers::types::SearchRequest;
use crate::AppState;

pub(super) async fn primary_outcome(
    primary: Vec<HybridSearchResult>,
    ctx: &QueryContext,
    payload: &SearchRequest,
    state: &Arc<AppState>,
) -> LaneOutcome {
    let results = match ctx.operation.policy {
        OperationPolicy::E4 => {
            loomem_core::search::context_selection::select(&payload.query, primary)
        }
        OperationPolicy::E5 => {
            loomem_core::search::context_relevance::select(&payload.query, primary)
        }
        OperationPolicy::E6 if budget_allows(state, ctx.stream_list.as_deref()) => {
            let reference = ctx.operation.reference_time.map(|value| value.to_rfc3339());
            let tracker = loomem_core::CostTracker::new(
                state.store.clone(),
                state.config.cost.clone(),
                state.http_client.clone(),
            );
            let stream_id = ctx
                .stream_list
                .as_deref()
                .and_then(|streams| streams.first())
                .map(String::as_str);
            loomem_core::search::semantic_context::select(
                &state.config.llm,
                &payload.query,
                reference.as_deref(),
                primary,
                loomem_core::search::semantic_context::SelectorAccounting {
                    tracker: &tracker,
                    stream_id,
                },
            )
            .await
        }
        _ => primary,
    };
    LaneOutcome {
        results,
        observation: None,
        #[cfg(feature = "evaluation")]
        evidence: None,
    }
}

fn budget_allows(state: &AppState, streams: Option<&[String]>) -> bool {
    if streams.is_some_and(|streams| streams.len() > 1) {
        return false;
    }
    let cap = state.config.cost.daily_cap_usd;
    if !check_cost_budget(&state.store, cap).allow_llm_calls() {
        return false;
    }
    streams.is_none_or(|streams| {
        streams
            .iter()
            .all(|stream| check_cost_budget_for_stream(&state.store, stream, cap).allow_llm_calls())
    })
}
