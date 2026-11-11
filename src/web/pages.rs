//! The owner's pages.

use askama::Template;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};

use super::{AppResult, AppState, ClientIp, json_error};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::{self, Actor};
use crate::passkey;
use webauthn_rs::prelude::PublicKeyCredential;

pub const VERSION: &str = env!("APP_VERSION");

pub fn routes() -> Router<AppState> {
    let mut r = Router::new()
        .route("/login", get(login))
        .route("/login/link/{secret}", get(link_confirm).post(link_use))
        .route("/login/passkey/start", post(passkey_start))
        .route("/login/passkey/finish", post(passkey_finish))
        .route("/logout", post(logout))
        .route("/api/whoami", get(whoami));
    for (path, _, milestone) in COMING {
        r = r.route(
            path,
            get(move |State(state): State<AppState>, Extension(actor): Extension<Actor>| {
                coming(state, actor, path, milestone)
            }),
        );
    }
    r
}

/// The navigation, in the order of the mockups.
const NAV: &[(&str, &str)] = &[
    ("/", "Dashboard"),
    ("/projects", "Projects"),
    ("/machine", "Machine"),
    ("/usage", "Usage"),
    ("/conversations", "Conversations"),
    ("/guides", "Guides"),
    ("/log", "Log"),
];

/// Pages that exist in the navigation but are built in a later milestone.
const COMING: &[(&str, &str, &str)] = &[
    ("/machine", "Machine", "M9"),
    ("/usage", "Usage", "M9"),
    ("/conversations", "Conversations", "M6"),
    ("/guides", "Guides", "M7"),
    ("/log", "Log", "M8"),
];

pub struct NavItem {
    pub href: &'static str,
    pub name: &'static str,
    pub active: bool,
}

/// What every page's header needs.
pub struct Nav {
    pub items: Vec<NavItem>,
    pub label: String,
    pub version: &'static str,
    pub waiting: i64,
}

/// The navigation with the live "waiting on you" count, for every page.
pub async fn nav(state: &AppState, actor: &Actor, active: &str) -> Nav {
    let mut n = Nav::new(actor, active);
    if actor.is_owner() {
        n.waiting = state.db.call(|c| crate::waiting::count(c)).await.unwrap_or(0) as i64;
    }
    n
}

impl Nav {
    pub fn new(actor: &Actor, active: &str) -> Self {
        Self {
            items: NAV.iter().map(|&(href, name)| NavItem { href, name, active: href == active }).collect(),
            label: actor.label(),
            version: VERSION,
            waiting: 0,
        }
    }
}

pub fn render(t: impl Template) -> AppResult<Response> {
    Ok(Html(t.render()?).into_response())
}

#[derive(Template)]
#[template(path = "coming.html")]
struct ComingPage {
    nav: Nav,
    title: &'static str,
    milestone: &'static str,
}

async fn coming(state: AppState, actor: Actor, path: &'static str, milestone: &'static str) -> AppResult<Response> {
    let title = COMING.iter().find(|c| c.0 == path).map(|c| c.1).unwrap_or("Page");
    render(ComingPage { nav: nav(&state, &actor, path).await, title, milestone })
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    version: &'static str,
    /// Set on the confirmation page of a one-time link.
    link: Option<String>,
    message: Option<String>,
    /// Passkeys work here and at least one is enrolled.
    passkeys: bool,
    /// Passkeys exist but this address can't use them (plain http on the WireGuard IP).
    passkey_elsewhere: bool,
}

impl LoginPage {
    fn new(link: Option<String>, message: Option<String>) -> Self {
        Self { version: VERSION, link, message, passkeys: false, passkey_elsewhere: false }
    }
}

async fn login(State(state): State<AppState>, Extension(actor): Extension<Actor>) -> AppResult<Response> {
    if actor.is_owner() {
        return Ok(Redirect::to("/").into_response());
    }
    let enrolled = state.db.call(|c| passkey::count(c)).await? > 0;
    let mut page = LoginPage::new(None, None);
    page.passkeys = enrolled && state.passkeys.available();
    page.passkey_elsewhere = enrolled && !state.passkeys.available();
    render(page)
}

/// Opening a link only shows a button; signing in takes a POST. Link previews (chat apps, mail
/// scanners) fetch links with GET, and must not use them up.
async fn link_confirm(Path(secret): Path<String>) -> AppResult<Response> {
    render(LoginPage::new(Some(secret), None))
}

async fn passkey_start(State(state): State<AppState>, Extension(ClientIp(ip)): Extension<ClientIp>) -> Response {
    let pk = state.passkeys.clone();
    match state.db.call(move |c| pk.start_login(c, &ip)).await {
        Ok(Some((id, options))) => Json(serde_json::json!({"id": id, "options": options})).into_response(),
        Ok(None) => json_error(StatusCode::BAD_REQUEST, "no passkey is enrolled yet"),
        Err(e) => json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

#[derive(serde::Deserialize)]
struct PasskeyFinish {
    id: String,
    credential: PublicKeyCredential,
}

async fn passkey_finish(
    State(state): State<AppState>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    headers: HeaderMap,
    jar: CookieJar,
    Json(body): Json<PasskeyFinish>,
) -> Response {
    let ua = user_agent(&headers);
    let limits = state.limits();
    let pk = state.passkeys.clone();
    let ip2 = ip.clone();
    let result = state
        .db
        .call(move |c| {
            let name = pk.finish_login(c, &body.id, &body.credential)?;
            Ok((name, auth::create_session(c, "passkey", &limits, ua.as_deref(), Some(&ip2))?))
        })
        .await;
    match result {
        Ok((name, session)) => {
            let e = Entry::new(
                &Actor::Anonymous,
                Via::Page,
                Action::Login,
                format!("owner signed in with passkey \"{name}\""),
            )
            .ip(Some(ip));
            activity::write(&state.db, e).await;
            let jar = jar.add(session_cookie(&state, session));
            (jar, Json(serde_json::json!({"ok": true}))).into_response()
        }
        Err(e) => {
            let entry =
                Entry::new(&Actor::Anonymous, Via::Page, Action::Denied, format!("passkey sign-in failed: {e:#}"))
                    .ip(Some(ip));
            activity::write(&state.db, entry).await;
            json_error(StatusCode::UNAUTHORIZED, "that passkey didn't work")
        }
    }
}

fn user_agent(headers: &HeaderMap) -> Option<String> {
    headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).map(|s| s.chars().take(300).collect::<String>())
}

async fn link_use(
    State(state): State<AppState>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    headers: HeaderMap,
    jar: CookieJar,
    Path(secret): Path<String>,
) -> AppResult<Response> {
    let ua =
        headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).map(|s| s.chars().take(200).collect::<String>());
    let limits = state.limits();
    let ip2 = ip.clone();
    let session = state
        .db
        .call(move |c| {
            let Some(_purpose) = auth::consume_login_link(c, &secret)? else { return Ok(None) };
            Ok(Some(auth::create_session(c, "link", &limits, ua.as_deref(), Some(&ip2))?))
        })
        .await?;
    let Some(session) = session else {
        let e = Entry::new(&Actor::Anonymous, Via::Page, Action::Denied, "sign-in link unknown, used or expired")
            .ip(Some(ip));
        activity::write(&state.db, e).await;
        let page = LoginPage::new(
            None,
            Some("This link has expired or was already used. Run `agentflow admin login-link` on the server for a new one.".into()),
        );
        return Ok((StatusCode::GONE, Html(page.render()?)).into_response());
    };
    let e =
        Entry::new(&Actor::Anonymous, Via::Page, Action::Login, "owner signed in with a one-time link").ip(Some(ip));
    activity::write(&state.db, e).await;
    let jar = jar.add(session_cookie(&state, session));
    Ok((jar, Redirect::to("/")).into_response())
}

fn session_cookie(state: &AppState, value: String) -> Cookie<'static> {
    Cookie::build((state.session_cookie_name(), value))
        .path("/")
        .http_only(true)
        .secure(state.config.is_https())
        .same_site(SameSite::Lax)
        .max_age(time::Duration::days(state.config.session_max_days))
        .build()
}

async fn logout(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    jar: CookieJar,
) -> AppResult<Response> {
    if let Actor::Owner { session_id, .. } = actor {
        state.db.call(move |c| auth::end_session(c, session_id)).await?;
    }
    let jar = jar.remove(Cookie::build(state.session_cookie_name()).path("/").build());
    Ok((jar, Redirect::to("/login")).into_response())
}

async fn whoami(Extension(actor): Extension<Actor>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "kind": actor.kind(),
        "name": actor.name(),
        "project": actor.project(),
        "version": VERSION,
    }))
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorPage<'a> {
    version: &'static str,
    status: u16,
    message: &'a str,
}

pub fn error_page(status: StatusCode, message: &str) -> Response {
    let page = ErrorPage { version: VERSION, status: status.as_u16(), message };
    match page.render() {
        Ok(html) => (status, Html(html)).into_response(),
        Err(_) => (status, message.to_owned()).into_response(),
    }
}
