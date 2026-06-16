use serde_json::{json, Value};
use std::sync::Arc;

use super::dispatcher;
use super::session::{self, SessionStore};
use super::tools;
use super::types::*;
use crate::auth::AuthContext;
use crate::AppState;

/// Route a JSON-RPC request. Returns None for notifications (no id).
pub async fn route_jsonrpc(
    state: &Arc<AppState>,
    sessions: &SessionStore,
    session_id: Option<&str>,
    request: JsonRpcRequest,
    auth: &AuthContext,
) -> Option<JsonRpcResponse> {
    let stream_id: &str = &auth.stream_id;
    if request.jsonrpc != "2.0" {
        return request.id.map(|id| {
            JsonRpcResponse::error(id, JsonRpcError::invalid_request("jsonrpc must be \"2.0\""))
        });
    }

    // Notifications (no id) don't get responses
    let id = match request.id {
        Some(id) => id,
        None => {
            // "notifications/initialized" — just ack silently
            return None;
        }
    };

    // Require session for everything except initialize and ping
    if request.method != "initialize" && request.method != "ping" {
        let has_session = match session_id {
            Some(sid) => session::get_session(sessions, sid).await,
            None => false,
        };
        if !has_session {
            return Some(JsonRpcResponse::error(
                id,
                JsonRpcError::invalid_request("Session not initialized. Send 'initialize' first."),
            ));
        }
    }

    match request.method.as_str() {
        "initialize" => Some(handle_initialize(state, id, request.params, stream_id)),
        "ping" => Some(JsonRpcResponse::success(id, json!({}))),
        "tools/list" => Some(handle_tools_list(id)),
        "tools/call" => Some(handle_tools_call(state, id, request.params, stream_id, auth).await),
        _ => Some(JsonRpcResponse::error(
            id,
            JsonRpcError::method_not_found(&request.method),
        )),
    }
}

// Single source of truth: loomem-server/mcp_instructions.md
// Edit that file, not this constant. Embedded at compile time via include_str!.
const MCP_INSTRUCTIONS: &str = include_str!("../../mcp_instructions.md");

fn append_advisory_section(out: &mut String, advisories: &[loomem_core::advisor::AdvisoryItem]) {
    out.push_str("\n\n## MEMORY ADVISORY\n");
    out.push_str(&format!(
        "There are {} active advisory items for this memory stream.\n",
        advisories.len()
    ));
    for adv in advisories {
        let priority = match adv.priority {
            loomem_core::advisor::AdvisoryPriority::High => "HIGH",
            loomem_core::advisor::AdvisoryPriority::Medium => "MEDIUM",
            loomem_core::advisor::AdvisoryPriority::Low => "LOW",
        };
        out.push_str(&format!("- [{}] {}\n", priority, adv.message));
    }
    out.push_str("\nConsider running memory_reflect or memory_dream to address these.\n");
}

fn build_instructions(advisories: &[loomem_core::advisor::AdvisoryItem]) -> String {
    let mut instructions = MCP_INSTRUCTIONS.to_string();
    if !advisories.is_empty() {
        append_advisory_section(&mut instructions, advisories);
    }
    instructions
}

fn handle_initialize(
    state: &Arc<AppState>,
    id: Value,
    _params: Option<Value>,
    stream_id: &str,
) -> JsonRpcResponse {
    // ECA-13: collect dynamic advisory items, then build the instructions.
    let advisories = if state.config.advisor.enabled {
        loomem_core::advisor::get_cached_advisories(&state.store, stream_id, 3)
    } else {
        Vec::new()
    };
    let instructions = build_instructions(&advisories);

    let result = McpInitializeResult {
        protocol_version: MCP_PROTOCOL_VERSION.into(),
        capabilities: ServerCapabilities {
            tools: ToolsCapability {
                list_changed: false,
            },
        },
        server_info: ServerInfo {
            name: "loomem-memory".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        },
        instructions: Some(instructions),
    };
    JsonRpcResponse::success(id, serde_json::to_value(result).unwrap())
}

fn handle_tools_list(id: Value) -> JsonRpcResponse {
    JsonRpcResponse::success(id, json!({ "tools": tools::tool_definitions() }))
}

async fn handle_tools_call(
    state: &Arc<AppState>,
    id: Value,
    params: Option<Value>,
    stream_id: &str,
    auth: &AuthContext,
) -> JsonRpcResponse {
    let params: ToolCallParams = match params {
        Some(v) => match serde_json::from_value(v) {
            Ok(p) => p,
            Err(e) => {
                return JsonRpcResponse::error(id, JsonRpcError::invalid_params(&e.to_string()))
            }
        },
        None => return JsonRpcResponse::error(id, JsonRpcError::invalid_params("missing params")),
    };

    match dispatcher::dispatch_tool(state, &params.name, params.arguments, stream_id, auth).await {
        Ok(result) => JsonRpcResponse::success(id, serde_json::to_value(result).unwrap()),
        Err(e) => JsonRpcResponse::error(id, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loomem_core::advisor::{AdvisoryItem, AdvisoryPriority, AdvisoryType};

    const PREAMBLE: &str = "You have access to Loomem";
    const TRANSPARENCY: &str = "Based on what I remember from previous conversations";

    #[test]
    fn test_initialize_instructions_render() {
        let out = build_instructions(&[]);
        assert!(out.contains(PREAMBLE), "preamble missing");
        assert!(out.contains(TRANSPARENCY), "transparency section missing");
        // No advisory section without advisories.
        assert!(!out.contains("## MEMORY ADVISORY"));
    }

    #[test]
    fn test_initialize_advisor_injection_preserved() {
        let advisories = vec![
            AdvisoryItem {
                id: "adv-1".into(),
                advisory_type: AdvisoryType::HealthCheck,
                message: "Memory pressure rising".into(),
                suggested_action: None,
                affected_chunk_ids: vec![],
                priority: AdvisoryPriority::High,
                created_at: 0,
            },
            AdvisoryItem {
                id: "adv-2".into(),
                advisory_type: AdvisoryType::RepeatedQuery,
                message: "Repeated query: \"login\"".into(),
                suggested_action: None,
                affected_chunk_ids: vec![],
                priority: AdvisoryPriority::Medium,
                created_at: 0,
            },
        ];
        let out = build_instructions(&advisories);
        assert!(out.contains(PREAMBLE), "preamble preserved");
        // Advisory section appended verbatim (ECA-13 contract).
        assert!(out.contains("## MEMORY ADVISORY"));
        assert!(out.contains("There are 2 active advisory items for this memory stream."));
        assert!(out.contains("- [HIGH] Memory pressure rising"));
        assert!(out.contains("- [MEDIUM] Repeated query: \"login\""));
        assert!(
            out.contains("Consider running memory_reflect or memory_dream to address these."),
            "advisory tail line preserved",
        );
    }
}
