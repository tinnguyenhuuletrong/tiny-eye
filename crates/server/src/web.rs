//! The web UI, compiled into the binary from `ui/dist` (see build.rs). Hashed assets are cached
//! for a year; a missing asset is a 404; every other path gets `index.html`, so client-side
//! routes survive a reload.

use axum::http::{StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Response};

include!(concat!(env!("OUT_DIR"), "/ui_assets.rs"));

/// Paths the UI owns: everything outside the API and the ingest endpoints.
pub fn is_ui_path(path: &str) -> bool {
    !["/api/", "/v1/", "/loki/"].iter().any(|p| path.starts_with(p))
}

pub async fn serve(uri: Uri) -> Response {
    let path = uri.path();
    if !is_ui_path(path) {
        return (StatusCode::NOT_FOUND, "no such API route; see GET /openapi.yaml\n").into_response();
    }
    if let Some((_, mime, body)) = UI_ASSETS.iter().find(|(p, _, _)| *p == path) {
        let cache = if path.starts_with("/assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
        return ([(header::CONTENT_TYPE, *mime), (header::CACHE_CONTROL, cache)], *body).into_response();
    }
    // A missing hashed asset (stale tab after an upgrade) must fail as itself, not load HTML.
    if path.starts_with("/assets/") {
        return (StatusCode::NOT_FOUND, "no such asset\n").into_response();
    }
    match UI_ASSETS.iter().find(|(p, _, _)| *p == "/index.html") {
        Some((_, mime, body)) => ([(header::CONTENT_TYPE, *mime), (header::CACHE_CONTROL, "no-cache")], *body).into_response(),
        None => Html(
            "<!doctype html><title>tiny-eye</title><body style=\"font:15px system-ui;margin:3rem auto;max-width:40rem;padding:0 1rem\">\
             <h1>tiny-eye</h1><p>The web UI is not built into this binary. Run <code>make ui</code> and rebuild \
             (<code>make build</code>), or use the API directly: <a href=\"/openapi.yaml\">/openapi.yaml</a>.</p>",
        )
        .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_assets_are_404_and_client_routes_get_the_shell() {
        assert_eq!(serve(Uri::from_static("/assets/index-gone.js")).await.status(), StatusCode::NOT_FOUND);
        assert_eq!(serve(Uri::from_static("/dashboards/shop")).await.status(), StatusCode::OK);
        assert_eq!(serve(Uri::from_static("/api/v1/nope")).await.status(), StatusCode::NOT_FOUND);
    }
}
