//! MCP 2026-07-28 stateless path (SEP-2575 / SEP-2243).
//!
//! Requests that carry `MCP-Protocol-Version: 2026-07-28` skip the
//! initialize/session machinery entirely: the mirrored headers and required
//! `_meta` fields are validated per request, then dispatch goes straight to
//! `router::route_stateless`. Requests with a legacy or absent version header
//! never reach this module — `classify_protocol_version` sends them down the
//! untouched legacy path in `handler.rs`.

use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::Value;
use std::sync::Arc;

use super::router;
use super::types::*;
use crate::auth::AuthContext;
use crate::AppState;

/// Where a request routes based on its `MCP-Protocol-Version` header.
#[derive(Debug, PartialEq, Eq)]
pub enum VersionRoute {
    /// No header, or an initialize-era version: the legacy session path.
    /// (The transport spec explicitly allows treating a missing header as
    /// 2025-03-26; 2025-06-18+ clients send their *negotiated* version.)
    Legacy,
    /// `2026-07-28`: the stateless path.
    Stateless,
    /// Anything else: reject with `-32004` and the supported list.
    Unsupported(String),
}

pub fn classify_protocol_version(header: Option<&str>) -> VersionRoute {
    match header {
        None => VersionRoute::Legacy,
        Some(v) if v == MCP_PROTOCOL_VERSION_2026_07_28 => VersionRoute::Stateless,
        Some(v) if LEGACY_PROTOCOL_VERSIONS.contains(&v) => VersionRoute::Legacy,
        Some(v) => VersionRoute::Unsupported(v.to_string()),
    }
}

/// The `id` to echo in error responses issued before a request is accepted
/// (`-32600`, `-32004`). JSON-RPC 2.0 allows only strings, numbers and null
/// as identifiers and mandates null when the id cannot be trusted, so an
/// object/array/boolean id maps to `Null` instead of being echoed back
/// (Greptile #63, verified repro).
pub fn request_id_of(body: &Value) -> Value {
    match body.get("id") {
        Some(id @ (Value::String(_) | Value::Number(_))) => id.clone(),
        _ => Value::Null,
    }
}

/// Handle one POST on the stateless path. The caller has already parsed the
/// body into JSON and classified the version header as `Stateless`.
pub async fn handle_stateless_post(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    body: Value,
    auth: &AuthContext,
) -> axum::response::Response {
    // JSON-RPC batching left the protocol in 2025-06-18; on this path the
    // body must be a single request or notification.
    if body.is_array() {
        return error_response(
            StatusCode::BAD_REQUEST,
            Value::Null,
            JsonRpcError::invalid_request("JSON-RPC batching is not supported in 2026-07-28"),
        );
    }
    let request = match parse_single_request(body) {
        Ok(r) => r,
        Err((id, e)) => return error_response(StatusCode::BAD_REQUEST, id, e),
    };
    // Notifications: accept with 202 and no body. Header requirements for
    // notification POSTs are undefined in this revision, so none are enforced.
    let Some(request_id) = request.id.clone() else {
        return StatusCode::ACCEPTED.into_response();
    };
    if let Err(e) = validate_mirrored_headers(headers, &request) {
        return error_response(StatusCode::BAD_REQUEST, request_id, e);
    }
    if let Err(e) = validate_request_meta(&request) {
        return error_response(StatusCode::BAD_REQUEST, request_id, e);
    }
    match router::route_stateless(state, request, auth).await {
        Some(response) => {
            let status = http_status_for(&response);
            (status, Json(response)).into_response()
        }
        // Unreachable in practice (notifications returned above), but a
        // response-free request still maps to 202 per the transport.
        None => StatusCode::ACCEPTED.into_response(),
    }
}

fn error_response(status: StatusCode, id: Value, error: JsonRpcError) -> axum::response::Response {
    (status, Json(JsonRpcResponse::error(id, error))).into_response()
}

/// Parse the POST body into a single JSON-RPC request. Valid JSON that is not
/// a valid request object maps to `-32600` invalid request — JSON parsing
/// already succeeded at this stage, so `-32700` would be wrong (Greptile #63
/// P1). The extractable `id`, if any, rides along for the error response.
fn parse_single_request(body: Value) -> Result<JsonRpcRequest, (Value, JsonRpcError)> {
    let body_id = request_id_of(&body);
    // Serde collapses an explicit `"id": null` into the same `None` as an
    // omitted id, but JSON-RPC 2.0 makes only the *omitted* id a
    // notification — a present null id is a (discouraged but legal) request
    // whose response id is null. Capture presence before the deserializer
    // erases it (Greptile #63, round 4).
    let has_explicit_null_id = body.get("id").is_some_and(Value::is_null);
    let mut request: JsonRpcRequest = serde_json::from_value(body)
        .map_err(|e| (body_id, JsonRpcError::invalid_request(&e.to_string())))?;
    if request.id.is_none() && has_explicit_null_id {
        request.id = Some(Value::Null);
    }
    // JSON-RPC 2.0 ids must be strings, numbers or null. A present id of any
    // other type is an invalid request answered with a null id — the given
    // id cannot be echoed (Greptile #63, verified repro).
    if let Some(id) = &request.id {
        if !(id.is_string() || id.is_number() || id.is_null()) {
            return Err((
                Value::Null,
                JsonRpcError::invalid_request("id must be a string, a number or null"),
            ));
        }
    }
    Ok(request)
}

/// Transport status for a stateless-path JSON-RPC response (SEP-2243):
/// unknown method → 404, internal errors → 500, other protocol errors this
/// path emits → 400, success (including tool-level `is_error` results) → 200.
fn http_status_for(response: &JsonRpcResponse) -> StatusCode {
    match &response.error {
        Some(e) if e.code == -32601 => StatusCode::NOT_FOUND,
        Some(e) if e.code == -32603 => StatusCode::INTERNAL_SERVER_ERROR,
        Some(_) => StatusCode::BAD_REQUEST,
        None => StatusCode::OK,
    }
}

/// SEP-2243 mirrored-header validation: `Mcp-Method` is required on every
/// request and must equal the body `method`; `Mcp-Name` is required on
/// `tools/call` and must equal `params.name` after sentinel decoding. Any
/// failure maps to `-32020` with HTTP 400.
pub fn validate_mirrored_headers(
    headers: &HeaderMap,
    request: &JsonRpcRequest,
) -> Result<(), JsonRpcError> {
    match headers.get("mcp-method").and_then(|v| v.to_str().ok()) {
        None => {
            return Err(JsonRpcError::header_mismatch(
                "Mcp-Method header is required",
            ))
        }
        Some(m) if m != request.method => {
            return Err(JsonRpcError::header_mismatch(&format!(
                "Mcp-Method header value '{}' does not match body method '{}'",
                m, request.method
            )));
        }
        Some(_) => {}
    }
    if request.method == "tools/call" {
        let body_name = request
            .params
            .as_ref()
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str());
        let Some(raw) = headers.get("mcp-name").and_then(|v| v.to_str().ok()) else {
            return Err(JsonRpcError::header_mismatch(
                "Mcp-Name header is required for tools/call",
            ));
        };
        let Some(decoded) = decode_header_value(raw) else {
            return Err(JsonRpcError::header_mismatch(
                "Mcp-Name header value is not valid Base64 sentinel encoding",
            ));
        };
        // A body without `params.name` is rejected as invalid params at
        // dispatch; only a *present but different* name is a mismatch here.
        if let Some(body_name) = body_name {
            if decoded != body_name {
                return Err(JsonRpcError::header_mismatch(&format!(
                    "Mcp-Name header value '{}' does not match body value '{}'",
                    decoded, body_name
                )));
            }
        }
    }
    Ok(())
}

/// SEP-2575 `_meta` validation: the three required fields must be present and
/// the embedded version must match the header (== "2026-07-28" on this path).
/// A missing field is malformed (`-32602`); a conflicting version is a
/// header/body mismatch (`-32020`). Both carry HTTP 400.
pub fn validate_request_meta(request: &JsonRpcRequest) -> Result<(), JsonRpcError> {
    let Some(meta) = request.params.as_ref().and_then(|p| p.get("_meta")) else {
        return Err(JsonRpcError::invalid_params(
            "params._meta with the io.modelcontextprotocol/* protocolVersion, clientInfo and clientCapabilities fields is required",
        ));
    };
    match meta.get(META_PROTOCOL_VERSION).and_then(|v| v.as_str()) {
        None => {
            return Err(JsonRpcError::invalid_params(&format!(
                "_meta[\"{}\"] is required",
                META_PROTOCOL_VERSION
            )));
        }
        Some(v) if v != MCP_PROTOCOL_VERSION_2026_07_28 => {
            return Err(JsonRpcError::header_mismatch(&format!(
                "MCP-Protocol-Version header value '{}' does not match _meta value '{}'",
                MCP_PROTOCOL_VERSION_2026_07_28, v
            )));
        }
        Some(_) => {}
    }
    // Shallow shape checks: `Implementation` internals (name/version) stay
    // the client's concern; the server only needs the objects to exist.
    if !meta.get(META_CLIENT_INFO).is_some_and(Value::is_object) {
        return Err(JsonRpcError::invalid_params(&format!(
            "_meta[\"{}\"] is required",
            META_CLIENT_INFO
        )));
    }
    if !meta
        .get(META_CLIENT_CAPABILITIES)
        .is_some_and(Value::is_object)
    {
        return Err(JsonRpcError::invalid_params(&format!(
            "_meta[\"{}\"] is required",
            META_CLIENT_CAPABILITIES
        )));
    }
    Ok(())
}

/// Decode a mirrored header value, honoring the transport's Base64 sentinel
/// format (`=?base64?<payload>?=`) used when the source value is not
/// header-safe ASCII. A plain value passes through unchanged.
fn decode_header_value(raw: &str) -> Option<String> {
    match raw
        .strip_prefix("=?base64?")
        .and_then(|r| r.strip_suffix("?="))
    {
        Some(payload) => base64_decode(payload).and_then(|bytes| String::from_utf8(bytes).ok()),
        None => Some(raw.to_string()),
    }
}

/// Minimal standard-alphabet Base64 decoder (RFC 4648 §4, padded input). Kept
/// in-tree for the same reason `oauth.rs` hand-rolls SHA-256: one small,
/// well-specified primitive doesn't justify a new dependency.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    fn digit(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes = input.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let chunk_count = bytes.len() / 4;
    let mut out = Vec::with_capacity(chunk_count * 3);
    for (index, chunk) in bytes.as_chunks::<4>().0.iter().enumerate() {
        let pad = match (chunk[2], chunk[3]) {
            (b'=', b'=') => 2,
            (_, b'=') => 1,
            _ => 0,
        };
        // Padding is only valid at the very end of the input.
        if pad > 0 && index + 1 != chunk_count {
            return None;
        }
        let mut acc: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            let v = if i >= 4 - pad { 0 } else { digit(c)? };
            acc = (acc << 6) | v;
        }
        out.push(u8::try_from((acc >> 16) & 0xFF).ok()?);
        if pad < 2 {
            out.push(u8::try_from((acc >> 8) & 0xFF).ok()?);
        }
        if pad < 1 {
            out.push(u8::try_from(acc & 0xFF).ok()?);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use serde_json::json;

    fn req(v: Value) -> JsonRpcRequest {
        serde_json::from_value(v).expect("test request")
    }

    // Literal keys on purpose: these tests break if the constants ever drift
    // from the wire format the spec fixes.
    fn full_meta() -> Value {
        json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": { "name": "test-client", "version": "1.0.0" },
            "io.modelcontextprotocol/clientCapabilities": {},
        })
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(*k, HeaderValue::from_str(v).expect("header value"));
        }
        map
    }

    // ── version negotiation matrix ──────────────────────────────────

    #[test]
    fn absent_header_routes_legacy() {
        assert_eq!(classify_protocol_version(None), VersionRoute::Legacy);
    }

    #[test]
    fn initialize_era_versions_route_legacy() {
        for v in ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"] {
            assert_eq!(
                classify_protocol_version(Some(v)),
                VersionRoute::Legacy,
                "{v} must stay on the untouched legacy path"
            );
        }
    }

    #[test]
    fn stateless_version_routes_stateless() {
        assert_eq!(
            classify_protocol_version(Some("2026-07-28")),
            VersionRoute::Stateless
        );
    }

    #[test]
    fn unknown_version_is_unsupported_with_supported_list() {
        let VersionRoute::Unsupported(v) = classify_protocol_version(Some("2027-01-01")) else {
            panic!("unknown version must classify as Unsupported");
        };
        let err = JsonRpcError::unsupported_protocol_version(&v);
        assert_eq!(err.code, UNSUPPORTED_PROTOCOL_VERSION);
        let data = err.data.expect("supported list");
        assert_eq!(data["supported"][0], "2026-07-28");
        assert_eq!(data["supported"][1], "2025-03-26");
        assert_eq!(data["requested"], "2027-01-01");
    }

    // ── SEP-2243 mirrored headers ───────────────────────────────────

    #[test]
    fn missing_mcp_method_is_header_mismatch() {
        let r = req(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}));
        let err = validate_mirrored_headers(&HeaderMap::new(), &r).expect_err("must fail");
        assert_eq!(err.code, HEADER_MISMATCH);
    }

    #[test]
    fn mismatched_mcp_method_is_header_mismatch() {
        let r = req(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}));
        let h = headers(&[("mcp-method", "tools/call")]);
        let err = validate_mirrored_headers(&h, &r).expect_err("must fail");
        assert_eq!(err.code, HEADER_MISMATCH);
    }

    #[test]
    fn matching_mcp_method_passes_for_non_call_methods() {
        let r = req(json!({"jsonrpc": "2.0", "id": 1, "method": "server/discover"}));
        let h = headers(&[("mcp-method", "server/discover")]);
        assert!(validate_mirrored_headers(&h, &r).is_ok());
    }

    #[test]
    fn tools_call_requires_mcp_name() {
        let r = req(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "memory_search", "arguments": {}}
        }));
        let h = headers(&[("mcp-method", "tools/call")]);
        let err = validate_mirrored_headers(&h, &r).expect_err("must fail");
        assert_eq!(err.code, HEADER_MISMATCH);
    }

    #[test]
    fn tools_call_with_matching_plain_name_passes() {
        let r = req(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "memory_search", "arguments": {}}
        }));
        let h = headers(&[("mcp-method", "tools/call"), ("mcp-name", "memory_search")]);
        assert!(validate_mirrored_headers(&h, &r).is_ok());
    }

    #[test]
    fn tools_call_name_decodes_base64_sentinel() {
        // "memory_search" in the transport's =?base64?…?= sentinel form.
        let r = req(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "memory_search", "arguments": {}}
        }));
        let h = headers(&[
            ("mcp-method", "tools/call"),
            ("mcp-name", "=?base64?bWVtb3J5X3NlYXJjaA==?="),
        ]);
        assert!(validate_mirrored_headers(&h, &r).is_ok());

        let h = headers(&[
            ("mcp-method", "tools/call"),
            ("mcp-name", "=?base64?not-base64!?="),
        ]);
        let err = validate_mirrored_headers(&h, &r).expect_err("must fail");
        assert_eq!(err.code, HEADER_MISMATCH);
    }

    #[test]
    fn tools_call_with_conflicting_name_is_header_mismatch() {
        let r = req(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "memory_search", "arguments": {}}
        }));
        let h = headers(&[("mcp-method", "tools/call"), ("mcp-name", "memory_store")]);
        let err = validate_mirrored_headers(&h, &r).expect_err("must fail");
        assert_eq!(err.code, HEADER_MISMATCH);
    }

    // ── SEP-2575 `_meta` requirements ───────────────────────────────

    #[test]
    fn missing_meta_is_invalid_params() {
        let r = req(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}));
        let err = validate_request_meta(&r).expect_err("must fail");
        assert_eq!(err.code, -32602);
    }

    #[test]
    fn each_required_meta_field_is_enforced() {
        for missing in [
            META_PROTOCOL_VERSION,
            META_CLIENT_INFO,
            META_CLIENT_CAPABILITIES,
        ] {
            let mut meta = full_meta();
            meta.as_object_mut().expect("object").remove(missing);
            let r = req(json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/list",
                "params": {"_meta": meta}
            }));
            let err = validate_request_meta(&r).expect_err("must fail");
            assert_eq!(err.code, -32602, "missing {missing} must be invalid params");
        }
    }

    #[test]
    fn meta_version_conflicting_with_header_is_header_mismatch() {
        let mut meta = full_meta();
        meta[META_PROTOCOL_VERSION] = json!("2025-03-26");
        let r = req(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list",
            "params": {"_meta": meta}
        }));
        let err = validate_request_meta(&r).expect_err("must fail");
        assert_eq!(err.code, HEADER_MISMATCH);
    }

    #[test]
    fn complete_meta_passes() {
        let r = req(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/list",
            "params": {"_meta": full_meta()}
        }));
        assert!(validate_request_meta(&r).is_ok());
    }

    // ── plumbing ────────────────────────────────────────────────────

    #[test]
    fn base64_decoder_roundtrips_and_rejects() {
        assert_eq!(base64_decode("aGVsbG8="), Some(b"hello".to_vec()));
        assert_eq!(
            base64_decode("bWVtb3J5X3NlYXJjaA=="),
            Some(b"memory_search".to_vec())
        );
        assert_eq!(base64_decode("abc"), None, "length must be a multiple of 4");
        assert_eq!(base64_decode("aG=s"), None, "inner padding is invalid");
        assert_eq!(
            base64_decode("aGVsbG8=aaaa"),
            None,
            "padding only at the end"
        );
    }

    #[test]
    fn request_id_extraction_defaults_to_null() {
        assert_eq!(request_id_of(&json!({"id": 7, "method": "x"})), json!(7));
        assert_eq!(request_id_of(&json!({"id": "abc"})), json!("abc"));
        assert_eq!(request_id_of(&json!([1, 2])), Value::Null);
        // JSON-RPC ids are strings, numbers or null — an invalid id type is
        // never echoed back into an error response (null instead).
        assert_eq!(request_id_of(&json!({"id": {"o": 1}})), Value::Null);
        assert_eq!(request_id_of(&json!({"id": [1]})), Value::Null);
        assert_eq!(request_id_of(&json!({"id": true})), Value::Null);
        assert_eq!(request_id_of(&json!({"id": null})), Value::Null);
    }

    #[test]
    fn non_request_json_is_invalid_request_not_parse_error() {
        // Valid JSON with an invalid JSON-RPC shape: parsing already
        // succeeded, so the stateless path answers -32600, not -32700,
        // echoing the id when one is extractable (Greptile #63 P1).
        let (id, err) = parse_single_request(json!({"id": 5, "not": "a request"}))
            .expect_err("shape must be rejected");
        assert_eq!(err.code, -32600);
        assert_eq!(id, json!(5));
        let (id, err) = parse_single_request(json!("just a string")).expect_err("must fail");
        assert_eq!(err.code, -32600);
        assert_eq!(id, Value::Null);
    }

    #[test]
    fn invalid_id_type_is_rejected_with_null_id() {
        // JSON-RPC ids are strings, numbers or null; an object/array/bool id
        // is an invalid request answered with a null id, before header or
        // meta validation runs (Greptile #63, verified repro).
        for bad in [json!({"bad": true}), json!([1]), json!(true)] {
            let (id, err) = parse_single_request(json!({
                "jsonrpc": "2.0", "id": bad, "method": "tools/list", "params": {}
            }))
            .expect_err("invalid id type must be rejected");
            assert_eq!(err.code, -32600);
            assert_eq!(id, Value::Null);
        }
        // Legal id types still parse.
        assert!(
            parse_single_request(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
                .is_ok()
        );
        assert!(
            parse_single_request(json!({"jsonrpc": "2.0", "id": "a", "method": "tools/list"}))
                .is_ok()
        );
    }

    #[test]
    fn explicit_null_id_is_a_request_not_a_notification() {
        // JSON-RPC 2.0: only an *omitted* id makes a notification; an
        // explicit null id is a legal request whose response id is null
        // (Greptile #63, round 4).
        let r = parse_single_request(
            json!({"jsonrpc": "2.0", "id": null, "method": "tools/list", "params": {}}),
        )
        .expect("null id parses as a request");
        assert_eq!(r.id, Some(Value::Null));
        let r = parse_single_request(json!({"jsonrpc": "2.0", "method": "tools/list"}))
            .expect("omitted id parses");
        assert_eq!(r.id, None, "omitted id stays a notification");
    }

    #[test]
    fn http_status_mapping_follows_transport_spec() {
        let ok = JsonRpcResponse::success(json!(1), json!({}));
        assert_eq!(http_status_for(&ok), StatusCode::OK);
        let not_found = JsonRpcResponse::error(json!(1), JsonRpcError::method_not_found("ping"));
        assert_eq!(http_status_for(&not_found), StatusCode::NOT_FOUND);
        let bad = JsonRpcResponse::error(json!(1), JsonRpcError::invalid_params("x"));
        assert_eq!(http_status_for(&bad), StatusCode::BAD_REQUEST);
        let internal = JsonRpcResponse::error(json!(1), JsonRpcError::internal("x"));
        assert_eq!(
            http_status_for(&internal),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
