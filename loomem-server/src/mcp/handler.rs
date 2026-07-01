use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::Value;
use std::sync::Arc;

use super::router;
use super::session;
use super::types::*;
use crate::auth::AuthContext;
use crate::AppState;

pub async fn mcp_post_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: axum::extract::Request,
) -> impl IntoResponse {
    let auth = match request.extensions().get::<AuthContext>().cloned() {
        Some(ctx) => ctx,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(
                    serde_json::to_value(JsonRpcResponse::error(
                        Value::Null,
                        JsonRpcError::invalid_request("Unauthorized"),
                    ))
                    .unwrap(),
                ),
            )
                .into_response()
        }
    };

    let body: Value = match axum::body::to_bytes(request.into_body(), 1024 * 1024).await {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(
                        serde_json::to_value(JsonRpcResponse::error(
                            Value::Null,
                            JsonRpcError::parse_error(),
                        ))
                        .unwrap(),
                    ),
                )
                    .into_response()
            }
        },
        Err(_) => return (StatusCode::BAD_REQUEST, Json(Value::Null)).into_response(),
    };
    let session_id = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Parse as single request or batch
    let requests: Vec<JsonRpcRequest> = if body.is_array() {
        match serde_json::from_value(body) {
            Ok(reqs) => reqs,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(
                        serde_json::to_value(JsonRpcResponse::error(
                            Value::Null,
                            JsonRpcError::parse_error(),
                        ))
                        .unwrap(),
                    ),
                )
                    .into_response()
            }
        }
    } else {
        match serde_json::from_value(body) {
            Ok(req) => vec![req],
            Err(_) => {
                return (
                    StatusCode::OK,
                    Json(
                        serde_json::to_value(JsonRpcResponse::error(
                            Value::Null,
                            JsonRpcError::parse_error(),
                        ))
                        .unwrap(),
                    ),
                )
                    .into_response()
            }
        }
    };

    let mut responses = Vec::new();
    let mut new_session_id: Option<String> = None;

    // Auto-recover stale sessions: if client sends a session ID that no longer exists
    // (e.g. after server restart), create a new session transparently.
    // mcp-remote proxies cache old session IDs and don't re-initialize on their own.
    let session_id = if let Some(ref sid) = session_id {
        if session::get_session(&state.mcp_sessions, sid).await {
            session_id // valid, keep it
        } else {
            let recovered = session::create_session(&state.mcp_sessions).await;
            new_session_id = Some(recovered.clone());
            Some(recovered)
        }
    } else {
        session_id
    };

    for request in requests {
        let is_initialize = request.method == "initialize";

        let sid_ref = if is_initialize {
            let sid = session::create_session(&state.mcp_sessions).await;
            new_session_id = Some(sid.clone());
            Some(sid)
        } else {
            session_id.clone()
        };

        if let Some(resp) = router::route_jsonrpc(
            &state,
            &state.mcp_sessions,
            sid_ref.as_deref(),
            request,
            &auth,
        )
        .await
        {
            responses.push(resp);
        }
    }

    // MCP Streamable HTTP: a POST carrying only notifications/responses (no
    // requests needing a reply) must be answered with 202 Accepted and no body.
    // Returning 200 with an empty JSON array `[]` breaks strict clients
    // (e.g. Codex) that reject `[]` as an invalid JSON-RPC message.
    let mut response = if responses.is_empty() {
        StatusCode::ACCEPTED.into_response()
    } else {
        let body = if responses.len() == 1 {
            serde_json::to_value(&responses[0]).unwrap()
        } else {
            serde_json::to_value(&responses).unwrap()
        };
        (StatusCode::OK, Json(body)).into_response()
    };
    if let Some(sid) = new_session_id.or(session_id) {
        response.headers_mut().insert(
            axum::http::header::HeaderName::from_static("mcp-session-id"),
            axum::http::header::HeaderValue::from_str(&sid)
                .unwrap_or_else(|_| axum::http::header::HeaderValue::from_static("")),
        );
    }
    response
}

pub async fn mcp_delete_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(sid) = headers.get("mcp-session-id").and_then(|v| v.to_str().ok()) {
        session::remove_session(&state.mcp_sessions, sid).await;
    }
    StatusCode::OK
}
