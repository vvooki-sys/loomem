use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── JSON-RPC 2.0 ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcResponse {
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Value, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

impl JsonRpcError {
    pub fn parse_error() -> Self {
        Self {
            code: -32700,
            message: "Parse error".into(),
            data: None,
        }
    }
    pub fn invalid_request(detail: &str) -> Self {
        Self {
            code: -32600,
            message: format!("Invalid request: {}", detail),
            data: None,
        }
    }
    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("Method not found: {}", method),
            data: None,
        }
    }
    pub fn invalid_params(detail: &str) -> Self {
        Self {
            code: -32602,
            message: format!("Invalid params: {}", detail),
            data: None,
        }
    }
    #[allow(dead_code)] // MCP JSON-RPC error constructors; internal() reserved for future error handling
    pub fn internal(detail: &str) -> Self {
        Self {
            code: -32603,
            message: format!("Internal error: {}", detail),
            data: None,
        }
    }
}

// ── MCP Protocol ──────────────────────────────────────────────────

pub const MCP_PROTOCOL_VERSION: &str = "2025-03-26";

#[allow(dead_code)] // MCP protocol negotiate types; deserialized but not yet inspected post-init
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpInitializeParams {
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: Value,
    pub client_info: Option<ClientInfo>,
}

#[allow(dead_code)] // MCP client info; received in init handshake, surfaced in future telemetry
#[derive(Debug, Deserialize, Clone)]
pub struct ClientInfo {
    pub name: String,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpInitializeResult {
    pub protocol_version: String,
    pub capabilities: ServerCapabilities,
    pub server_info: ServerInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ServerCapabilities {
    pub tools: ToolsCapability,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsCapability {
    pub list_changed: bool,
}

#[derive(Debug, Serialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

// ── Tool Call ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ToolCallParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub content: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

impl ToolResult {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock {
                content_type: "text".into(),
                text: s.into(),
            }],
            is_error: None,
        }
    }

    pub fn error(s: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock {
                content_type: "text".into(),
                text: s.into(),
            }],
            is_error: Some(true),
        }
    }
}
