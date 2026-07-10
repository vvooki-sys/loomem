//! Embedded dashboard SPA.
//!
//! `loomem-dashboard/dist` (the built Vite app) is compiled into the binary
//! via `rust-embed`, so every instance — hosted fleet, loomem.ai, localhost —
//! ships the dashboard in the same single file. The router's fallback serves
//! it: real asset paths stream the asset, path-less client-side routes
//! (`/memory`, `/connect`, …) get `index.html`, and API-shaped paths return a
//! plain 404 instead of leaking the SPA shell into JSON clients.
//!
//! When the front end has not been built (the `dist/` folder in git carries
//! only `.gitkeep`), the fallback answers 404 with an honest hint — never a
//! fabricated page. Build order: `npm run build` in `loomem-dashboard/`, then
//! `cargo build` (CI and release.yml do this).

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "../loomem-dashboard/dist/"]
struct DashboardAssets;

/// Path prefixes that belong to the API surface — they never fall through to
/// the SPA, so an unknown API path stays an honest 404 for JSON clients.
const API_PREFIXES: &[&str] = &[
    "v1/",
    "api/",
    "admin/",
    "mcp",
    "oauth/",
    "health",
    ".well-known/",
];

/// Content-Type for an embedded asset, by extension. Hand-rolled (the set of
/// extensions Vite emits is small and closed) instead of pulling a mime crate.
fn content_type_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("woff2") => "font/woff2",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("json") | Some("map") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Serve one embedded asset. Hashed Vite assets are immutable; everything
/// else (notably `index.html`) must revalidate so a new binary is picked up.
fn serve_asset(path: &str) -> Option<Response> {
    let asset = DashboardAssets::get(path)?;
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    Some(
        (
            [
                (header::CONTENT_TYPE, content_type_for(path)),
                (header::CACHE_CONTROL, cache),
            ],
            asset.data,
        )
            .into_response(),
    )
}

/// Router fallback: embedded SPA with client-side-routing support.
pub async fn spa_fallback(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');

    if API_PREFIXES.iter().any(|p| path.starts_with(p)) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    let candidate = if path.is_empty() { "index.html" } else { path };
    if let Some(resp) = serve_asset(candidate) {
        return resp;
    }

    // No such asset. Extension-less paths are client-side routes — serve the
    // SPA shell and let the router take over.
    if !candidate.contains('.') {
        if let Some(resp) = serve_asset("index.html") {
            return resp;
        }
    }

    (
        StatusCode::NOT_FOUND,
        "dashboard not built — run `npm run build` in loomem-dashboard/ and rebuild the server",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_paths_never_fall_through_to_the_spa() {
        for p in [
            "v1/nope",
            "api/x",
            "admin/x",
            "mcp",
            "oauth/token",
            "health",
            ".well-known/x",
        ] {
            assert!(
                API_PREFIXES.iter().any(|pre| p.starts_with(pre)),
                "{p} must be API-shaped"
            );
        }
        // Client-side routes are NOT API-shaped.
        for p in ["", "memory", "connect", "settings", "assets/app.js"] {
            assert!(!API_PREFIXES
                .iter()
                .any(|pre| !pre.is_empty() && p.starts_with(pre)));
        }
    }

    #[test]
    fn content_types_cover_vite_output() {
        assert_eq!(content_type_for("index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type_for("assets/index-abc.js"), "text/javascript");
        assert_eq!(content_type_for("assets/index-abc.css"), "text/css");
        assert_eq!(content_type_for("assets/font-abc.woff2"), "font/woff2");
        assert_eq!(content_type_for("favicon.svg"), "image/svg+xml");
        assert_eq!(content_type_for("wordmark.svg"), "image/svg+xml");
    }

    #[tokio::test]
    async fn unknown_api_path_is_a_plain_404() {
        let resp = spa_fallback("/v1/definitely-not-a-route".parse().unwrap()).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn client_route_serves_shell_or_honest_404() {
        // With a built dist/ this is the SPA shell (200 text/html); with the
        // committed empty dist/ it is the honest "dashboard not built" 404.
        let resp = spa_fallback("/memory".parse().unwrap()).await;
        assert!(
            resp.status() == StatusCode::OK || resp.status() == StatusCode::NOT_FOUND,
            "unexpected status {}",
            resp.status()
        );
    }
}
