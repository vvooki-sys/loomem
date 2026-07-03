use axum::{
    extract::Request,
    http::{header, StatusCode},
    middleware::Next,
    response::Response,
};
pub use loomem_core::storage::UserRole;
use loomem_core::storage::DEFAULT_STREAM_ID;
use subtle::ConstantTimeEq;

/// Which API-key scope resolved the caller. Single-user deployments always
/// authenticate as `Shared` (the one API key); `Private` is kept for the
/// dispatcher's per-stream bookkeeping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyScope {
    Shared,
    Private,
}

/// One stream the caller has access to within the current request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamMembership {
    /// Storage stream id (e.g. `__user_default__`).
    pub stream_id: String,

    /// Effective role ON THIS STREAM. Single user = `UserRole::Admin`.
    pub role: UserRole,

    /// Which auth vector exposed this membership.
    pub source: KeyScope,
}

/// Auth context injected into every request after middleware.
#[derive(Clone, Debug)]
pub struct AuthContext {
    pub stream_id: String,
    pub user_id: Option<String>,
    pub is_admin: bool, // kept for backward compat — true if role is any admin
    pub role: UserRole,
    pub scope: KeyScope,
    /// All streams the caller currently has access to. Invariant: always
    /// contains an entry whose `stream_id` matches `self.stream_id`.
    pub memberships: Vec<StreamMembership>,
}

impl AuthContext {
    /// Build an AuthContext for a caller with a single stream membership —
    /// the default/active one. Keeps the `memberships contains stream_id`
    /// invariant trivially.
    ///
    /// Param order: `stream_id`, `role` (effective on that stream), `scope`
    /// (source of access), `user_id`, `is_admin`.
    #[must_use]
    pub fn single_stream(
        stream_id: impl Into<String>,
        role: UserRole,
        scope: KeyScope,
        user_id: Option<String>,
        is_admin: bool,
    ) -> Self {
        let stream_id = stream_id.into();
        let memberships = vec![StreamMembership {
            stream_id: stream_id.clone(),
            role,
            source: scope,
        }];
        Self {
            stream_id,
            user_id,
            is_admin,
            role,
            scope,
            memberships,
        }
    }
}

/// Config passed to middleware via extensions.
#[derive(Clone)]
pub struct AuthConfig {
    /// The single API key. `None` disables auth (local passthrough mode).
    pub admin_token: Option<String>,
}

/// Bearer token auth middleware.
///
/// Single-user model: one API key, passed as `Authorization: Bearer <key>`.
/// When no key is configured, every request passes through as admin
/// (local development mode).
pub async fn auth_middleware(mut request: Request, next: Next) -> Result<Response, StatusCode> {
    let config = request.extensions().get::<AuthConfig>().cloned();

    let bearer = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.to_string());

    let admin_ctx = || {
        AuthContext::single_stream(
            DEFAULT_STREAM_ID,
            UserRole::Admin,
            KeyScope::Shared,
            None,
            true,
        )
    };

    let Some(config) = config else {
        // No auth config — passthrough (local mode)
        request.extensions_mut().insert(admin_ctx());
        return Ok(next.run(request).await);
    };

    // No token configured → auth disabled
    let Some(ref admin_token) = config.admin_token else {
        request.extensions_mut().insert(admin_ctx());
        return Ok(next.run(request).await);
    };

    match bearer {
        // Audit 2026-07-01 item 4: constant-time comparison so the token
        // cannot be probed byte-by-byte via response timing. `ct_eq` on
        // slices short-circuits only on length, which is not secret here.
        Some(token) if bool::from(token.as_bytes().ct_eq(admin_token.as_bytes())) => {
            request.extensions_mut().insert(admin_ctx());
            Ok(next.run(request).await)
        }
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// Stream ids become RocksDB key components (e.g. `access:{stream}:…`) matched
/// by byte prefix, so a value containing `:` or control characters could
/// collide across namespaces or escape a prefix (audit F5). Restrict to a
/// conservative charset + length; every reserved (`__…__`), user, project,
/// UUID-shaped, and numeric stream id in use satisfies it.
#[must_use]
pub fn is_valid_stream_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Validate that the requested stream belongs to the authenticated user's scope.
/// Admins can access any stream. Regular users can only access their own stream_id.
/// Returns the validated stream, defaulting to auth.stream_id if none requested.
pub fn validate_stream(auth: &AuthContext, requested: Option<&str>) -> Result<String, StatusCode> {
    let stream = requested.unwrap_or(&auth.stream_id);
    if !is_valid_stream_id(stream) {
        tracing::warn!(
            target: "audit",
            "Rejected malformed stream id: user={:?} requested={:?}",
            auth.user_id, stream
        );
        return Err(StatusCode::BAD_REQUEST);
    }
    if auth.is_admin {
        return Ok(stream.to_string());
    }
    if stream == auth.stream_id {
        Ok(stream.to_string())
    } else {
        tracing::warn!(
            target: "audit",
            "Cross-stream access denied: user={:?} requested={} owned={}",
            auth.user_id, stream, auth.stream_id
        );
        Err(StatusCode::FORBIDDEN)
    }
}

/// Validate multiple streams against auth scope. Each must belong to the user.
pub fn validate_streams(
    auth: &AuthContext,
    requested: Option<&[String]>,
) -> Result<Option<Vec<String>>, StatusCode> {
    match requested {
        None => Ok(Some(vec![auth.stream_id.clone()])),
        Some(streams) => {
            for s in streams {
                validate_stream(auth, Some(s))?;
            }
            Ok(Some(streams.to_vec()))
        }
    }
}

// Legacy wrapper type — kept for backward compat with existing code
#[allow(dead_code)] // backward-compat wrapper; referenced by future token-based auth paths
#[derive(Clone)]
pub struct AuthToken(pub String);

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    async fn echo_ctx(ctx: Option<axum::Extension<AuthContext>>) -> String {
        match ctx {
            Some(axum::Extension(c)) => format!(
                "stream={} admin={} role={:?}",
                c.stream_id, c.is_admin, c.role
            ),
            None => "no-ctx".to_string(),
        }
    }

    fn app(cfg: AuthConfig) -> Router {
        Router::new().route("/probe", get(echo_ctx)).route_layer({
            let ac = cfg;
            axum::middleware::from_fn(
                move |mut req: axum::extract::Request, next: axum::middleware::Next| {
                    req.extensions_mut().insert(ac.clone());
                    auth_middleware(req, next)
                },
            )
        })
    }

    fn req(path: &str, token: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().uri(path);
        if let Some(t) = token {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        b.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn valid_token_passes_as_admin() {
        let app = app(AuthConfig {
            admin_token: Some("secret".into()),
        });
        let resp = app.oneshot(req("/probe", Some("secret"))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn invalid_token_rejected() {
        let app = app(AuthConfig {
            admin_token: Some("secret".into()),
        });
        let resp = app.oneshot(req("/probe", Some("wrong"))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // Audit 2026-07-01 item 4: comparison is constant-time; a same-length
    // wrong token must still be rejected (401), same as before.
    #[tokio::test]
    async fn same_length_wrong_token_rejected() {
        let app = app(AuthConfig {
            admin_token: Some("secret".into()),
        });
        let resp = app.oneshot(req("/probe", Some("secreX"))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn missing_token_rejected_when_configured() {
        let app = app(AuthConfig {
            admin_token: Some("secret".into()),
        });
        let resp = app.oneshot(req("/probe", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn passthrough_when_no_token_configured() {
        let app = app(AuthConfig { admin_token: None });
        let resp = app.oneshot(req("/probe", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn validate_stream_admin_any() {
        let ctx = AuthContext::single_stream(
            DEFAULT_STREAM_ID,
            UserRole::Admin,
            KeyScope::Shared,
            None,
            true,
        );
        assert_eq!(
            validate_stream(&ctx, Some("other_stream")).unwrap(),
            "other_stream"
        );
    }

    #[test]
    fn validate_stream_non_admin_own_only() {
        let ctx = AuthContext::single_stream(
            "my_stream",
            UserRole::Writer,
            KeyScope::Shared,
            Some("u1".into()),
            false,
        );
        assert_eq!(validate_stream(&ctx, None).unwrap(), "my_stream");
        assert_eq!(
            validate_stream(&ctx, Some("my_stream")).unwrap(),
            "my_stream"
        );
        assert_eq!(
            validate_stream(&ctx, Some("other")).unwrap_err(),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn single_stream_invariant_holds() {
        let ctx = AuthContext::single_stream(
            DEFAULT_STREAM_ID,
            UserRole::Admin,
            KeyScope::Shared,
            None,
            true,
        );
        assert!(ctx.memberships.iter().any(|m| m.stream_id == ctx.stream_id));
    }

    #[test]
    fn stream_id_charset_accepts_real_and_rejects_malformed() {
        // Every stream-id shape actually used by the system/clients passes.
        assert!(is_valid_stream_id("__user_default__"));
        assert!(is_valid_stream_id("__shared_team__"));
        assert!(is_valid_stream_id("__project_alpha__"));
        assert!(is_valid_stream_id("plej-lukasz-gumowski"));
        assert!(is_valid_stream_id("001"));
        assert!(is_valid_stream_id("550e8400-e29b-41d4-a716-446655440000"));
        // Malformed: empty, colon (prefix escape), whitespace, control, overlong.
        assert!(!is_valid_stream_id(""));
        assert!(!is_valid_stream_id("access:evil"));
        assert!(!is_valid_stream_id("has space"));
        assert!(!is_valid_stream_id("bad\nnewline"));
        assert!(!is_valid_stream_id(&"x".repeat(129)));
    }

    #[test]
    fn validate_stream_rejects_malformed_id() {
        let ctx = AuthContext::single_stream(
            DEFAULT_STREAM_ID,
            UserRole::Admin,
            KeyScope::Shared,
            None,
            true,
        );
        // Even an admin (who may target any stream) cannot pass a colon-bearing id.
        assert_eq!(
            validate_stream(&ctx, Some("access:evil")).unwrap_err(),
            StatusCode::BAD_REQUEST
        );
    }
}
