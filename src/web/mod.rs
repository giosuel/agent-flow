//! The HTTP server: the owner's pages, the JSON API, and the MCP endpoint, all behind one
//! authentication step.

mod api;
mod assets;
mod buttons;
mod dashboard;
mod icons;
mod pages;
mod projects;
mod pwa;
mod settings;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;

use crate::activity::{self, Action, Entry, Via};
use crate::auth::{self, Actor, SessionLimits};
use crate::config::Config;
use crate::db::Db;
use crate::github::GitHub;
use crate::passkey::Passkeys;
use crate::push::{Pusher, Vapid};

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub config: Arc<Config>,
    pub passkeys: Passkeys,
    pub push: Pusher,
    pub github: Arc<dyn GitHub>,
    /// Denied requests logged per client address in the current minute (flood guard).
    denied: Arc<std::sync::Mutex<std::collections::HashMap<String, (i64, u32)>>>,
    /// Running `/api/wait` calls per project.
    pub waits: Arc<std::sync::Mutex<std::collections::HashMap<i64, u32>>>,
}

impl AppState {
    pub fn new(db: Db, config: Config, vapid: Vapid, github: Arc<dyn GitHub>) -> Self {
        let passkeys = Passkeys::new(&config);
        let push = Pusher::new(db.clone(), vapid, &config.push_contact);
        Self { db, config: Arc::new(config), passkeys, push, github, denied: Arc::default(), waits: Arc::default() }
    }

    /// The owner's session cookie. On https it's a `__Host-` cookie: never shared with another
    /// host or path. On plain http (the WireGuard address) cookies aren't separated by port, so
    /// the port goes into the name, keeping a dev instance and the live one apart.
    pub fn session_cookie_name(&self) -> String {
        if self.config.is_https() {
            "__Host-af_session".into()
        } else {
            format!("af_session_{}", self.config.listen.port())
        }
    }

    /// At most 20 logged denials per address and minute; the rest only count as noise.
    fn log_denial(&self, ip: &str) -> bool {
        let minute = crate::util::now_ms() / 60_000;
        let mut map = self.denied.lock().unwrap();
        if map.len() > 10_000 {
            map.clear();
        }
        let e = map.entry(ip.to_owned()).or_insert((minute, 0));
        if e.0 != minute {
            *e = (minute, 0);
        }
        e.1 += 1;
        e.1 <= 20
    }

    pub fn limits(&self) -> SessionLimits {
        SessionLimits {
            idle_ms: self.config.session_idle_days * 86_400_000,
            max_ms: self.config.session_max_days * 86_400_000,
        }
    }
}

/// The client's address as the log shows it: the peer, or Caddy's X-Forwarded-For when the peer
/// is a trusted proxy.
#[derive(Debug, Clone)]
pub struct ClientIp(pub String);

pub fn router(state: AppState) -> Router {
    let mcp = crate::mcp::service(state.clone());
    Router::new()
        .merge(pages::routes())
        .merge(settings::routes())
        .merge(projects::routes())
        .merge(api::routes())
        .merge(buttons::routes())
        .merge(dashboard::routes())
        .merge(pwa::routes())
        .nest_service("/mcp", mcp)
        .merge(assets::routes())
        .layer(middleware::from_fn_with_state(state.clone(), log_requests))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

pub async fn serve(state: AppState) -> anyhow::Result<()> {
    let addr = state.config.listen;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(
        "AgentFlow {} listening on http://{addr} (public URL {})",
        env!("APP_VERSION"),
        state.config.public_url
    );
    let recovered = state.db.call(|c| crate::actions::recover_interrupted(c)).await?;
    if recovered > 0 {
        tracing::warn!("{recovered} button(s) were running when AgentFlow stopped; marked failed");
    }
    crate::poller::spawn(state.clone());
    let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
    tokio::select! {
        _ = ctrl_c => {},
        _ = term.recv() => {},
    }
}

/// Paths anyone may load: the sign-in pages and what they need to render.
fn is_public(path: &str) -> bool {
    path == "/login" || path.starts_with("/login/") || assets::is_asset(path) || pwa::is_public(path)
}

/// Pages and API calls that don't get a row of their own in the activity log. MCP tool calls log
/// themselves, with the tool name and record, which says more than "POST /mcp".
fn is_unlogged(path: &str) -> bool {
    assets::is_asset(path) || pwa::is_public(path) || path == "/mcp" || path.starts_with("/mcp/")
}

fn client_ip(config: &Config, peer: Option<IpAddr>, headers: &HeaderMap) -> String {
    let Some(peer) = peer else { return "unknown".into() };
    if config.trusted_proxies.contains(&peer)
        && let Some(forwarded) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok())
        && let Some(last) = forwarded.rsplit(',').next()
    {
        return last.trim().to_owned();
    }
    peer.to_string()
}

/// Works out the identity of every request and refuses anonymous ones.
async fn authenticate(State(state): State<AppState>, jar: CookieJar, mut req: Request, next: Next) -> Response {
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip());
    let ip = client_ip(&state.config, peer, req.headers());
    req.extensions_mut().insert(ClientIp(ip.clone()));
    let path = req.uri().path().to_owned();

    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t.trim().to_owned());
    let cookie = jar.get(&state.session_cookie_name()).map(|c| c.value().to_owned());
    let via_cookie = bearer.is_none() && cookie.is_some();

    let limits = state.limits();
    let from = ip.clone();
    let looked_up = state
        .db
        .call(move |c| match (bearer, cookie) {
            (Some(token), _) => auth::token_actor(c, &token, &from).map(|a| (a, true)),
            (None, Some(secret)) => auth::session_actor(c, &secret, &limits).map(|a| (a, false)),
            (None, None) => Ok((None, false)),
        })
        .await;
    let (actor, tried_token) = match looked_up {
        Ok(found) => found,
        Err(e) => {
            tracing::error!("authentication lookup failed: {e:#}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let actor = match actor {
        Some(a) => a,
        None if is_public(&path) => Actor::Anonymous,
        None => {
            if tried_token && state.log_denial(&ip) {
                let e = Entry::new(
                    &Actor::Anonymous,
                    via_for(&path),
                    Action::Denied,
                    format!("{} {path}: unknown or revoked token", req.method()),
                )
                .status(401)
                .ip(Some(ip));
                activity::write(&state.db, e).await;
            }
            return unauthenticated(&path, wants_html(req.headers()));
        }
    };

    // A browser session may only change things from AgentFlow's own pages, so another site can't
    // make the owner's browser press a button (CSRF). Tokens aren't sent by browsers on their own.
    if via_cookie && !is_safe(req.method()) && !same_origin(&state.config, req.headers()) {
        let e = Entry::new(
            &actor,
            Via::Page,
            Action::Denied,
            format!("{} {path}: request from another site", req.method()),
        )
        .status(403)
        .ip(Some(ip));
        activity::write(&state.db, e).await;
        return (StatusCode::FORBIDDEN, "This request didn't come from AgentFlow's own page.").into_response();
    }

    req.extensions_mut().insert(actor);
    next.run(req).await
}

fn is_safe(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

fn same_origin(config: &Config, headers: &HeaderMap) -> bool {
    let expected = config.public_origin();
    let listen_origin = format!("http://{}", config.listen);
    let ok = |v: &str| v == expected || v == listen_origin;
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        return ok(origin);
    }
    // Some browsers leave Origin off same-origin form posts; fall back to Referer.
    headers.get(header::REFERER).and_then(|v| v.to_str().ok()).is_some_and(|r| {
        ok(r)
            || r.strip_prefix(&expected).is_some_and(|rest| rest.starts_with('/'))
            || r.strip_prefix(&listen_origin).is_some_and(|rest| rest.starts_with('/'))
    })
}

fn wants_html(headers: &HeaderMap) -> bool {
    headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|a| a.contains("text/html"))
}

fn unauthenticated(path: &str, html: bool) -> Response {
    if html && !path.starts_with("/api/") && !path.starts_with("/mcp") {
        return Redirect::to("/login").into_response();
    }
    let mut res =
        (StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({"error": "sign in or send a token"}))).into_response();
    res.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    res
}

fn via_for(path: &str) -> Via {
    if path.starts_with("/mcp") {
        Via::Mcp
    } else if path.starts_with("/api/") {
        Via::Api
    } else {
        Via::Page
    }
}

/// One activity-log row per request: who, what, result and how long it took.
async fn log_requests(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    if is_unlogged(&path) {
        return next.run(req).await;
    }
    let method = req.method().clone();
    let actor = req.extensions().get::<Actor>().cloned().unwrap_or(Actor::Anonymous);
    let ip = req.extensions().get::<ClientIp>().map(|c| c.0.clone());
    let via = if actor.kind() == "device" { Via::Cli } else { via_for(&path) };
    let started = Instant::now();
    let res = next.run(req).await;
    let action = match (&method, path.as_str()) {
        (_, p) if p.starts_with("/login") || p == "/logout" => Action::Login,
        (m, _) if is_safe(m) => Action::Read,
        _ => Action::Write,
    };
    let status = res.status();
    // Strangers probing for pages that don't exist aren't worth a row each.
    if status == StatusCode::NOT_FOUND && matches!(actor, Actor::Anonymous) {
        return res;
    }
    let mut e = Entry::new(&actor, via, action, format!("{method} {path}"))
        .status(status.as_u16())
        .duration_ms(started.elapsed().as_millis() as i64)
        .ip(ip);
    if status.is_server_error() {
        e = e.severity("error");
    } else if status == StatusCode::FORBIDDEN {
        e = e.severity("warn");
    }
    activity::write(&state.db, e).await;
    res
}

async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
             connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    // Pages show tokens, answers and the like: never keep them in a cache. Static files set
    // their own caching.
    if !h.contains_key(header::CACHE_CONTROL) {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    res
}

/// A JSON error for API calls and the pages' scripts.
pub fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, axum::Json(serde_json::json!({"error": message.into()}))).into_response()
}

/// A return address from a form, if it's one of our own pages: a path, plain printable ASCII,
/// not `//…` or `/\…` (both mean another host to a browser). Anything else is ignored.
pub fn local_path(next: Option<&str>) -> Option<String> {
    let n = next?.trim();
    let ok = n.starts_with('/')
        && !n.starts_with("//")
        && n.len() <= 300
        && n.bytes().all(|b| b.is_ascii_graphic() && b != b'\\');
    ok.then(|| n.to_owned())
}

/// Errors a handler can return; each becomes a page (or JSON for API calls) with the right status.
#[derive(Debug)]
pub enum AppError {
    NotFound,
    Forbidden(&'static str),
    BadRequest(String),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        match e.downcast::<auth::Forbidden>() {
            Ok(f) => AppError::Forbidden(f.0),
            Err(e) => AppError::Internal(e),
        }
    }
}

impl From<auth::Forbidden> for AppError {
    fn from(f: auth::Forbidden) -> Self {
        AppError::Forbidden(f.0)
    }
}

impl From<askama::Error> for AppError {
    fn from(e: askama::Error) -> Self {
        AppError::Internal(e.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            AppError::NotFound => (StatusCode::NOT_FOUND, "Not found.".to_owned()),
            AppError::Forbidden(m) => (StatusCode::FORBIDDEN, m.to_owned()),
            AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            AppError::Internal(e) => {
                tracing::error!("{e:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong; it's in the server log.".to_owned())
            }
        };
        pages::error_page(status, &msg)
    }
}

pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests;
