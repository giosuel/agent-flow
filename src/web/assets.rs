//! Static files compiled into the binary: styles, scripts, fonts and icons. No CDN: the page works
//! with nothing but AgentFlow reachable.

use axum::Router;
use axum::extract::Path;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use super::AppState;

struct Asset {
    path: &'static str,
    mime: &'static str,
    body: &'static [u8],
}

macro_rules! asset {
    ($path:literal, $mime:literal) => {
        Asset { path: $path, mime: $mime, body: include_bytes!(concat!("../../static/", $path)) }
    };
}

const ASSETS: &[Asset] = &[
    asset!("app.css", "text/css; charset=utf-8"),
    asset!("app.js", "text/javascript; charset=utf-8"),
    asset!("logo.svg", "image/svg+xml"),
    asset!("log.js", "text/javascript; charset=utf-8"),
    asset!("machine.js", "text/javascript; charset=utf-8"),
    asset!("nav.js", "text/javascript; charset=utf-8"),
    asset!("passkey.js", "text/javascript; charset=utf-8"),
    asset!("settings.js", "text/javascript; charset=utf-8"),
    asset!("icons/apple-touch-icon.png", "image/png"),
    asset!("icons/icon-192.png", "image/png"),
    asset!("icons/icon-512.png", "image/png"),
    asset!("icons/maskable-512.png", "image/png"),
    asset!("fonts/plex-sans-400.woff2", "font/woff2"),
    asset!("fonts/plex-sans-500.woff2", "font/woff2"),
    asset!("fonts/plex-sans-600.woff2", "font/woff2"),
    asset!("fonts/plex-mono-400.woff2", "font/woff2"),
    asset!("fonts/plex-mono-500.woff2", "font/woff2"),
];

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/static/{*path}", get(static_file))
        .route("/favicon.svg", get(|| async { serve("logo.svg", false) }))
}

pub fn is_asset(path: &str) -> bool {
    path.starts_with("/static/") || path == "/favicon.svg"
}

async fn static_file(Path(path): Path<String>) -> Response {
    serve(&path, true)
}

fn serve(path: &str, immutable: bool) -> Response {
    let Some(asset) = ASSETS.iter().find(|a| a.path == path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut res = asset.body.into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(asset.mime));
    // Pages link assets with ?v=<version>, so a new release changes every URL and the files can be
    // cached for good.
    let cache = if immutable { "public, max-age=31536000, immutable" } else { "public, max-age=86400" };
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    res
}
