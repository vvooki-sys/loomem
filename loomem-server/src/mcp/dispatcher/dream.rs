use super::*;

pub(super) async fn tool_dream(
    state: &Arc<AppState>,
    args: Value,
    stream_id: &str,
) -> Result<ToolResult, JsonRpcError> {
    let _args: DreamArgs = serde_json::from_value(args).unwrap_or(DreamArgs { stream: None });
    if !state.config.dream.enabled {
        return Ok(ToolResult::text("Dream worker is disabled in config."));
    }

    let cost_tracker = loomem_core::CostTracker::new(
        state.store.clone(),
        state.config.cost.clone(),
        state.http_client.clone(),
    );

    match loomem_core::dream::dream_run(
        &state.store,
        &state.tantivy,
        &state.http_client,
        loomem_core::dream::DreamRunContext {
            llm_config: &state.config.llm,
            dream_config: &state.config.dream,
            pii_filter: &state.pii_filter,
            intent_log: state.intent_log.as_deref(),
            embedding_queue: state.embedding_queue.clone(),
        },
        &cost_tracker,
        stream_id,
    )
    .await
    {
        Ok(result) => {
            let text = format!(
                "Dream consolidation complete:\n- Chunks processed: {}\n- Subject groups: {}\n- Facts merged: {}\n- Contradictions resolved: {}\n- Cost: ${:.3}{}",
                result.chunks_processed,
                result.groups_found,
                result.facts_merged,
                result.contradictions_resolved,
                result.cost_usd,
                if result.cost_cap_reached { "\n⚠ Cost cap reached" } else { "" },
            );
            Ok(ToolResult::text(text))
        }
        Err(e) => Ok(ToolResult::error(format!("Dream failed: {}", e))),
    }
}
