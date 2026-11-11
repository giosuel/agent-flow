//! The home-screen app: web app manifest and service worker. Both are public (the browser fetches
//! them on its own) and contain no data.

use axum::Router;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use super::AppState;

const SERVICE_WORKER: &str = include_str!("../../static/sw.js");

pub fn routes() -> Router<AppState> {
    Router::new().route("/manifest.webmanifest", get(manifest)).route("/sw.js", get(service_worker))
}

pub fn is_public(path: &str) -> bool {
    path == "/manifest.webmanifest" || path == "/sw.js"
}

async fn manifest() -> Response {
    let v = env!("APP_VERSION");
    let body = serde_json::json!({
        "name": "AgentFlow",
        "short_name": "AgentFlow",
        "description": "Your agents' questions, merges and builds",
        "start_url": "/",
        "scope": "/",
        "display": "standalone",
        "background_color": "#000000",
        "theme_color": "#0A0A0C",
        "icons": [
            {"src": format!("/static/icons/icon-192.png?v={v}"), "sizes": "192x192", "type": "image/png"},
            {"src": format!("/static/icons/icon-512.png?v={v}"), "sizes": "512x512", "type": "image/png"},
            {"src": format!("/static/icons/maskable-512.png?v={v}"), "sizes": "512x512", "type": "image/png", "purpose": "maskable"},
        ],
    });
    let mut res = body.to_string().into_response();
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/manifest+json"));
    res
}

async fn service_worker() -> Response {
    let mut res = SERVICE_WORKER.into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/javascript; charset=utf-8"));
    // Browsers check for a new worker on every visit; never let a cache hold an old one.
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    res
}
