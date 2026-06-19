//! Minimal OAuth 2.0 layer for MCP Remote Connector compatibility.
//!
//! Implements just enough OAuth to let Claude.ai (and other MCP clients)
//! authenticate via the standard connector flow:
//!
//! 1. Client hits /mcp → 401
//! 2. Client reads /.well-known/oauth-protected-resource
//! 3. Client reads /.well-known/oauth-authorization-server
//! 4. Client registers via /oauth/register (Dynamic Client Registration)
//! 5. Client redirects user to /oauth/authorize
//! 6. User enters their Loomem API key → redirect back with auth code
//! 7. Client exchanges code for token via /oauth/token
//! 8. Client uses token as Bearer for /mcp
//!
//! The issued access_token IS the user's API key — no extra token layer.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

// ── In-memory stores ────────────────────────────────────────────────

/// Registered OAuth clients (from Dynamic Client Registration).
#[allow(dead_code)] // OAuth DCR fields; stored for future token-introspection and revocation endpoints
#[derive(Clone, Debug)]
pub struct OAuthClient {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub redirect_uris: Vec<String>,
}

/// Pending authorization: after user submits API key on /oauth/authorize.
#[allow(dead_code)] // PendingAuth fields read by future token-exchange validation
#[derive(Clone, Debug)]
struct PendingAuth {
    code: String,
    client_id: String,
    redirect_uri: String,
    api_key: String, // the user's Loomem API key — becomes the access_token
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    created_at: std::time::Instant,
}

#[derive(Clone)]
pub struct OAuthState {
    clients: Arc<RwLock<HashMap<String, OAuthClient>>>,
    pending_auths: Arc<RwLock<HashMap<String, PendingAuth>>>,
    pub server_origin: String, // e.g. "https://memory.example.com"
}

impl OAuthState {
    pub fn new(server_origin: String) -> Self {
        Self {
            clients: Arc::new(RwLock::new(HashMap::new())),
            pending_auths: Arc::new(RwLock::new(HashMap::new())),
            server_origin,
        }
    }

    /// Spawn a background task to clean up expired auth codes (>10 min old).
    pub fn spawn_cleanup(self: &Arc<Self>) {
        let state = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                let mut auths = state.pending_auths.write().await;
                auths.retain(|_, v| v.created_at.elapsed().as_secs() < 600);
            }
        });
    }
}

// ── Request / Response types ────────────────────────────────────────

#[derive(Deserialize)]
pub struct RegisterRequest {
    pub redirect_uris: Option<Vec<String>>,
    pub client_name: Option<String>,
    pub grant_types: Option<Vec<String>>,
    pub response_types: Option<Vec<String>>,
    pub token_endpoint_auth_method: Option<String>,
}

#[derive(Serialize)]
struct RegisterResponse {
    client_id: String,
    client_secret: String,
    redirect_uris: Vec<String>,
    client_name: Option<String>,
    grant_types: Vec<String>,
    response_types: Vec<String>,
    token_endpoint_auth_method: String,
}

#[derive(Deserialize)]
#[allow(dead_code)]
pub struct AuthorizeQuery {
    pub client_id: String,
    pub redirect_uri: String,
    pub response_type: Option<String>,
    pub state: Option<String>,
    pub scope: Option<String>,
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
    /// RFC 8707 resource indicator — passed by some MCP clients.
    pub resource: Option<String>,
}

#[derive(Deserialize)]
pub struct AuthorizeSubmit {
    pub client_id: String,
    pub redirect_uri: String,
    pub state: Option<String>,
    pub api_key: String,
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
pub struct TokenRequest {
    pub grant_type: String,
    pub code: Option<String>,
    pub redirect_uri: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub code_verifier: Option<String>,
}

// ── Handlers ────────────────────────────────────────────────────────

/// GET /.well-known/oauth-protected-resource
/// RFC 9728 — tells the client where to find the authorization server.
pub async fn protected_resource_metadata(
    State(state): State<Arc<OAuthState>>,
) -> impl IntoResponse {
    Json(json!({
        "resource": state.server_origin,
        "authorization_servers": [state.server_origin],
        "bearer_methods_supported": ["header"],
    }))
}

/// GET /.well-known/oauth-authorization-server
/// RFC 8414 — OAuth authorization server metadata.
pub async fn authorization_server_metadata(
    State(state): State<Arc<OAuthState>>,
) -> impl IntoResponse {
    let origin = &state.server_origin;
    Json(json!({
        "issuer": origin,
        "authorization_endpoint": format!("{}/oauth/authorize", origin),
        "token_endpoint": format!("{}/oauth/token", origin),
        "registration_endpoint": format!("{}/oauth/register", origin),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code"],
        "token_endpoint_auth_methods_supported": ["client_secret_post", "none"],
        "code_challenge_methods_supported": ["S256", "plain"],
        "scopes_supported": ["mcp"],
    }))
}

/// POST /oauth/register — Dynamic Client Registration (RFC 7591)
pub async fn register(
    State(state): State<Arc<OAuthState>>,
    Json(req): Json<RegisterRequest>,
) -> impl IntoResponse {
    let client_id = Uuid::new_v4().to_string();
    let client_secret = Uuid::new_v4().to_string();
    let redirect_uris = req.redirect_uris.unwrap_or_default();

    let client = OAuthClient {
        client_id: client_id.clone(),
        client_secret: Some(client_secret.clone()),
        redirect_uris: redirect_uris.clone(),
    };

    state
        .clients
        .write()
        .await
        .insert(client_id.clone(), client);

    (
        StatusCode::CREATED,
        Json(RegisterResponse {
            client_id,
            client_secret,
            redirect_uris,
            client_name: req.client_name,
            grant_types: req
                .grant_types
                .unwrap_or_else(|| vec!["authorization_code".into()]),
            response_types: req.response_types.unwrap_or_else(|| vec!["code".into()]),
            token_endpoint_auth_method: req
                .token_endpoint_auth_method
                .unwrap_or_else(|| "client_secret_post".into()),
        }),
    )
}

/// GET /oauth/authorize — shows a minimal login page where user enters API key.
pub async fn authorize_page(
    State(_state): State<Arc<OAuthState>>,
    Query(q): Query<AuthorizeQuery>,
) -> axum::response::Response {
    authorize_page_html(q).into_response()
}

/// Render the API key authorization form.
fn authorize_page_html(q: AuthorizeQuery) -> impl IntoResponse {
    let html = format!(
        r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Loomem — Connect</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Fraunces:opsz,wght@9..144,500;9..144,600&family=Inter:wght@400;500;600&display=swap" rel="stylesheet">
<style>
  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  body {{ font-family: 'Inter', -apple-system, 'Segoe UI', sans-serif; background: #FBF8F1;
          color: #1F1B16; display: flex; align-items: center; justify-content: center;
          min-height: 100vh; padding: 20px; }}
  .card {{ background: #fff; border: 1px solid #DED7C8; border-radius: 16px; max-width: 440px;
           width: 100%; overflow: hidden; box-shadow: 0 8px 24px rgba(31,27,22,.09); }}
  .bar {{ height: 5px; background: linear-gradient(120deg, #EE9913 0%, #1684DC 100%); }}
  .inner {{ padding: 32px 34px; }}
  .brand {{ display: flex; align-items: center; gap: 10px; font-family: 'Fraunces', Georgia, serif;
            font-weight: 600; font-size: 24px; letter-spacing: -.02em; margin-bottom: 22px; }}
  .brand svg {{ width: 38px; height: 38px; }}
  h1 {{ font-family: 'Fraunces', Georgia, serif; font-size: 1.45rem; font-weight: 600;
        letter-spacing: -.01em; margin-bottom: 8px; }}
  p {{ font-size: .95rem; color: #564D40; line-height: 1.55; margin-bottom: 22px; }}
  label {{ font-size: .85rem; font-weight: 600; color: #423B31; display: block; margin-bottom: 8px; }}
  input[type="password"] {{ width: 100%; padding: 13px 16px; border: 1.5px solid #B7AE9E;
         border-radius: 999px; background: #fff; color: #1F1B16; font-size: 15px;
         font-family: inherit; margin-bottom: 18px; }}
  input[type="password"]:focus {{ outline: none; border-color: #1684DC; box-shadow: 0 0 0 3px rgba(22,132,220,.25); }}
  button {{ width: 100%; padding: 14px; border: none; border-radius: 999px;
            background: linear-gradient(120deg, #EE9913 0%, #1684DC 100%); color: #fff;
            font-size: 16px; font-weight: 600; cursor: pointer; font-family: inherit; }}
  button:hover {{ filter: brightness(1.05); }}
  .fine {{ font-size: 12.5px; color: #8E8474; margin-top: 16px; text-align: center; }}
</style>
</head>
<body>
<div class="card">
  <div class="bar"></div>
  <div class="inner">
    <div class="brand">
      <svg viewBox="27 27 146 146" fill="none"><g stroke-linecap="round" fill="none" stroke-width="13"><circle cx="100" cy="100" r="66" stroke="#1684DC" stroke-dasharray="86 329" transform="rotate(187.5 100 100)"/><circle cx="100" cy="100" r="66" stroke="#1684DC" stroke-dasharray="86 329" transform="rotate(277.5 100 100)"/><circle cx="100" cy="100" r="66" stroke="#F4AC2E" stroke-dasharray="86 329" transform="rotate(7.5 100 100)"/><circle cx="100" cy="100" r="66" stroke="#F4AC2E" stroke-dasharray="86 329" transform="rotate(97.5 100 100)"/><circle cx="100" cy="100" r="50" stroke="#F4AC2E" stroke-dasharray="48 266" transform="rotate(182.5 100 100)"/><circle cx="100" cy="100" r="50" stroke="#1684DC" stroke-dasharray="48 266" transform="rotate(12.5 100 100)"/></g></svg>
      Loomem
    </div>
    <h1>Connect your context</h1>
    <p>Paste your key to connect Claude to your private Loomem. You'll find it in the email we sent you.</p>
    <form method="POST" action="/oauth/authorize">
      <input type="hidden" name="client_id" value="{client_id}">
      <input type="hidden" name="redirect_uri" value="{redirect_uri}">
      <input type="hidden" name="state" value="{state}">
      <input type="hidden" name="code_challenge" value="{code_challenge}">
      <input type="hidden" name="code_challenge_method" value="{code_challenge_method}">
      <label for="api_key">Your key</label>
      <input type="password" id="api_key" name="api_key" placeholder="Paste your key here" required autofocus>
      <button type="submit">Authorize</button>
    </form>
    <div class="fine">Your data stays private — only you can access it.</div>
  </div>
</div>
</body>
</html>"##,
        client_id = html_escape(&q.client_id),
        redirect_uri = html_escape(&q.redirect_uri),
        state = html_escape(&q.state.unwrap_or_default()),
        code_challenge = html_escape(&q.code_challenge.unwrap_or_default()),
        code_challenge_method = html_escape(&q.code_challenge_method.unwrap_or_default()),
    );
    Html(html)
}

/// POST /oauth/authorize — user submitted API key, redirect back with auth code.
pub async fn authorize_submit(
    State(state): State<Arc<OAuthState>>,
    axum::Form(form): axum::Form<AuthorizeSubmit>,
) -> impl IntoResponse {
    let code = Uuid::new_v4().to_string();

    let pending = PendingAuth {
        code: code.clone(),
        client_id: form.client_id,
        redirect_uri: form.redirect_uri.clone(),
        api_key: form.api_key,
        code_challenge: if form.code_challenge.as_deref() == Some("") {
            None
        } else {
            form.code_challenge
        },
        code_challenge_method: if form.code_challenge_method.as_deref() == Some("") {
            None
        } else {
            form.code_challenge_method
        },
        created_at: std::time::Instant::now(),
    };

    state
        .pending_auths
        .write()
        .await
        .insert(code.clone(), pending);

    // Build redirect URL with code + state
    let mut redirect_url = form.redirect_uri;
    let separator = if redirect_url.contains('?') { '&' } else { '?' };
    redirect_url = format!("{}{}code={}", redirect_url, separator, urlencoding(&code));
    if let Some(s) = form.state {
        if !s.is_empty() {
            redirect_url = format!("{}&state={}", redirect_url, urlencoding(&s));
        }
    }

    Redirect::to(&redirect_url)
}

/// POST /oauth/token — exchange authorization code for access token.
pub async fn token(
    State(state): State<Arc<OAuthState>>,
    axum::Form(req): axum::Form<TokenRequest>,
) -> impl IntoResponse {
    if req.grant_type != "authorization_code" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "unsupported_grant_type" })),
        )
            .into_response();
    }

    let code = match &req.code {
        Some(c) => c.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid_request", "error_description": "missing code" })),
            )
                .into_response()
        }
    };

    // Look up and consume the auth code
    let pending = state.pending_auths.write().await.remove(&code);
    let pending = match pending {
        Some(p) => p,
        None => return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({ "error": "invalid_grant", "error_description": "unknown or expired code" }),
            ),
        )
            .into_response(),
    };

    // Check code expiry (10 minutes)
    if pending.created_at.elapsed().as_secs() > 600 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_grant", "error_description": "code expired" })),
        )
            .into_response();
    }

    // Verify PKCE if code_challenge was provided
    if let Some(challenge) = &pending.code_challenge {
        let verifier = match &req.code_verifier {
            Some(v) => v,
            None => return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid_grant", "error_description": "missing code_verifier" })),
            ).into_response(),
        };

        let method = pending.code_challenge_method.as_deref().unwrap_or("plain");
        let valid = match method {
            "S256" => {
                use std::io::Write;
                let mut hasher = Sha256::new();
                hasher.write_all(verifier.as_bytes()).ok();
                let hash = hasher.finalize();
                base64_url_encode(&hash) == *challenge
            }
            "plain" => verifier == challenge,
            _ => false,
        };

        if !valid {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid_grant", "error_description": "PKCE verification failed" })),
            ).into_response();
        }
    }

    // The access_token IS the user's API key
    (
        StatusCode::OK,
        Json(json!({
            "access_token": pending.api_key,
            "token_type": "Bearer",
            "scope": "mcp",
        })),
    )
        .into_response()
}

// ── Helpers ─────────────────────────────────────────────────────────

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn urlencoding(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                String::from(b as char)
            }
            _ => format!("%{:02X}", b),
        })
        .collect()
}

// ── Minimal SHA-256 (no extra deps) ─────────────────────────────────

/// Minimal SHA-256 implementation for PKCE S256 verification.
struct Sha256 {
    data: Vec<u8>,
}

impl Sha256 {
    fn new() -> Self {
        Self { data: Vec::new() }
    }

    fn finalize(self) -> Vec<u8> {
        sha256_digest(&self.data)
    }
}

impl std::io::Write for Sha256 {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.data.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn sha256_digest(data: &[u8]) -> Vec<u8> {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let k: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    // Pre-processing: padding
    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0x00);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    // Process each 64-byte block
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(k[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    h.iter().flat_map(|v| v.to_be_bytes()).collect()
}

fn base64_url_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut result = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((n >> 18) & 63) as usize] as char);
        result.push(CHARS[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((n >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            result.push(CHARS[(n & 63) as usize] as char);
        }
    }
    result
}
