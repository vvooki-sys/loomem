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
    /// SEP-2575: the requested protocol version is not implemented here.
    /// Carried with HTTP 400; `data` advertises what this server does speak.
    pub fn unsupported_protocol_version(requested: &str) -> Self {
        Self {
            code: UNSUPPORTED_PROTOCOL_VERSION,
            message: format!("Unsupported protocol version: {}", requested),
            data: Some(serde_json::json!({
                "supported": SUPPORTED_PROTOCOL_VERSIONS,
                "requested": requested,
            })),
        }
    }
    /// SEP-2243: a mirrored HTTP header is missing or does not match the
    /// request body. Carried with HTTP 400.
    pub fn header_mismatch(detail: &str) -> Self {
        Self {
            code: HEADER_MISMATCH,
            message: format!("Header mismatch: {}", detail),
            data: None,
        }
    }
    #[allow(dead_code)] // reserved like internal(): emitted once a request path requires client capabilities (MRTR); no Loomem path does today
    pub fn missing_client_capability(required: Value) -> Self {
        Self {
            code: MISSING_REQUIRED_CLIENT_CAPABILITY,
            message: "Missing required client capability".into(),
            data: Some(serde_json::json!({ "requiredCapabilities": required })),
        }
    }
}

// ── MCP Protocol ──────────────────────────────────────────────────

pub const MCP_PROTOCOL_VERSION: &str = "2025-03-26";

// ── MCP 2026-07-28 (stateless, SEP-2575) ──────────────────────────
// The 2026-07-28 revision removes the initialize handshake: every request
// carries protocol version, client info and client capabilities in `_meta`
// (mirrored into HTTP headers per SEP-2243), and `server/discover` replaces
// the handshake for discovery. Key names below were verified against the
// SEP-2575 text and the modelcontextprotocol.io transport/schema pages
// (2026-08-03); they live here as constants so a late schema correction
// lands in exactly one place.

/// The stateless revision served on the new path.
pub const MCP_PROTOCOL_VERSION_2026_07_28: &str = "2026-07-28";

/// Versions this server implements, newest first — advertised by
/// `server/discover` and in `-32004` error data.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 2] =
    [MCP_PROTOCOL_VERSION_2026_07_28, MCP_PROTOCOL_VERSION];

/// Initialize-era revisions. A request carrying one of these in
/// `MCP-Protocol-Version` (or carrying no header at all) takes the legacy
/// session path, byte-identical to pre-2026 behavior. 2025-06-18+ clients
/// send the header with their *negotiated* version, so these must route,
/// not error.
pub const LEGACY_PROTOCOL_VERSIONS: [&str; 4] = [
    "2024-11-05",
    MCP_PROTOCOL_VERSION,
    "2025-06-18",
    "2025-11-25",
];

/// Required per-request `_meta` keys (SEP-2575).
pub const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
pub const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";

/// JSON-RPC error codes introduced by the 2026-07-28 revision.
pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32004; // SEP-2575
pub const HEADER_MISMATCH: i64 = -32020; // SEP-2243
#[allow(dead_code)] // reserved with missing_client_capability() below
pub const MISSING_REQUIRED_CLIENT_CAPABILITY: i64 = -32003; // SEP-2575

/// SEP-2549 cache hints on `tools/list` — required fields on the 2026-07-28
/// path. The tool list is static per server config (it changes only on
/// deploy), so a long TTL keeps client prompt caches stable; `private`
/// because the endpoint is authenticated and descriptions could become
/// stream-dependent later.
pub const TOOLS_LIST_TTL_MS: u64 = 3_600_000;
pub const TOOLS_LIST_CACHE_SCOPE: &str = "private";

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

/// `server/discover` result (SEP-2575) — the stateless home of everything
/// `initialize` used to negotiate.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoverResult {
    pub supported_versions: Vec<String>,
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
