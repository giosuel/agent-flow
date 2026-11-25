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
    Router::new()
        .route("/login", get(login))
        .route("/login/link/{secret}", get(link_confirm).post(link_use))
        .route("/login/passkey/start", post(passkey_start))
        .route("/login/passkey/finish", post(passkey_finish))
        .route("/logout", post(logout))
        .route("/api/whoami", get(whoami))
        .route("/more", get(more))
}

/// The navigation, in the order of the mockups.
const NAV: &[(&str, &str)] = &[
    ("/", "Dashboard"),
    ("/projects", "Projects"),
    ("/machine", "Machine"),
    ("/usage", "Usage"),
    ("/conversations", "Conversations"),
    ("/handbook", "Handbook"),
    ("/log", "Log"),
];

pub struct NavItem {
    pub href: &'static str,
    pub name: &'static str,
    pub active: bool,
}

impl NavItem {
    /// Its icon in the desktop bar (the owner, 2026-11-19, D-232: the global pages as icon buttons).
    pub fn icon(&self) -> &'static str {
        super::icons::icon(match self.href {
            "/" => "grid",
            "/projects" => "folder",
            "/machine" => "machine",
            "/usage" => "chart",
            "/conversations" => "message",
            "/handbook" => "book",
            _ => "list",
        })
    }
}

/// A project in the desktop bar's switcher.
pub struct NavProject {
    pub slug: String,
    pub name: String,
    pub online: bool,
}

/// One of the project's tabs in the desktop bar.
pub struct BarTab {
    pub label: &'static str,
    pub href: String,
    /// Its count ("2"), or empty.
    pub count: String,
    /// The count asks for the owner (questions, to test): drawn in the warning colour.
    pub warn: bool,
    pub on: bool,
    /// Opens as a dialog (Settings).
    pub dlg: bool,
}

/// The project a page belongs to: the desktop bar shows its name in the switcher and its tabs.
pub struct Here {
    pub slug: String,
    pub name: String,
    pub tabs: Vec<BarTab>,
    /// The tab the page is under ("" on a page outside the project).
    pub tab: String,
}

impl Here {
    /// Where the switcher takes you in the project `slug`: the page you're on here, in that
    /// project (the owner, 2026-11-25, D-280: "when I am looking at imaginarium's notes and then
    /// switch to lituus, i want to immediately be on the lituus notes page"). From Settings (a
    /// dialog) and from pages outside a project, its overview.
    pub fn switch_to(&self, slug: &str) -> String {
        match self.tab.as_str() {
            "settings" => tab_href(slug, "overview"),
            tab => tab_href(slug, tab),
        }
    }
}

/// The address of the project `slug`'s tab `key` (a key of the project page's tabs).
pub fn tab_href(slug: &str, key: &str) -> String {
    match key {
        "" | "overview" => format!("/p/{slug}"),
        "plan" | "settings" => format!("/p/{slug}/{key}"),
        _ => format!("/p/{slug}?tab={key}"),
    }
}

/// On the phone a tab bar at the bottom, as in iOS apps and the owner's viber console (the owner,
/// 2026-11-11: "in the mobile version, it has the classic iOS menu at the bottom. can we also do
/// it like this?"): four sections and More, which lists the rest. iOS fits five tabs at most.
const TABS: &[(&str, &str, &str)] = &[
    ("/", "Dashboard", "grid"),
    ("/projects", "Projects", "folder"),
    ("/usage", "Usage", "chart"),
    ("/handbook", "Handbook", "book"),
];
pub const MORE: &[(&str, &str, &str)] = &[
    ("/machine", "Machine", "machine"),
    ("/conversations", "Conversations", "message"),
    ("/log", "Log", "list"),
    ("/settings", "Settings", "gear"),
];

/// Who is signed in, for the avatar at the top right (D-270).
pub struct Me {
    pub name: String,
    pub email: String,
    pub icon: String,
}

pub struct Tab {
    pub href: &'static str,
    pub name: &'static str,
    pub icon: &'static str,
    pub active: bool,
}

/// What every page's header needs.
pub struct Nav {
    pub items: Vec<NavItem>,
    pub tabs: Vec<Tab>,
    /// On the phone a sub-page shows a back bar ("‹ Handbook") instead of the logo, like an app's
    /// navigation (the owner, 2026-11-11: "clicking on something should open a new "window" or
    /// feel more like navigation").
    pub back: Option<(String, String)>,
    pub version: &'static str,
    pub waiting: i64,
    /// Notifications not read yet (the bell's badge).
    pub unread: i64,
    /// The projects for the desktop bar's switcher, not the archived ones.
    pub projects: Vec<NavProject>,
    /// The project the page is in, if any.
    pub here: Option<Here>,
    /// Who is signed in (None on pages without a person).
    pub me: Option<Me>,
}

/// The cookie that keeps the project last opened (the owner, 2026-11-23: "lets do cookie"): on
/// the pages outside the projects the desktop bar shows it, drawn here, so nothing flashes.
pub const PROJECT_COOKIE: &str = "af_project";

tokio::task_local! {
    /// The project the request's cookie keeps, for `nav`.
    static KEPT_PROJECT: Option<String>;
}

/// Around the owner's page requests: hands `nav` the kept project, and keeps the project a page
/// belongs to (`/p/<slug>…`) once it has loaded. The projects list (/projects, "All projects" in
/// the switcher) forgets it.
pub async fn keep_project(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let owner = req.extensions().get::<Actor>().is_some_and(|a| a.is_owner());
    if !owner || req.method() != axum::http::Method::GET {
        return next.run(req).await;
    }
    let kept = CookieJar::from_headers(req.headers()).get(PROJECT_COOKIE).map(|c| c.value().to_owned());
    let path = req.uri().path();
    let opened = path
        .strip_prefix("/p/")
        .map(|rest| rest.split('/').next().unwrap_or_default().to_owned())
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    let forget = path == "/projects";
    let mut res = KEPT_PROJECT.scope(kept.clone(), next.run(req)).await;
    if !res.status().is_success() {
        return res;
    }
    let cookie = match (opened, forget) {
        (Some(slug), _) if kept.as_deref() != Some(slug.as_str()) => Cookie::build((PROJECT_COOKIE, slug))
            .path("/")
            .http_only(true)
            .secure(state.config.is_https())
            .same_site(SameSite::Lax)
            .max_age(time::Duration::days(365))
            .build(),
        (None, true) if kept.is_some() => Cookie::build((PROJECT_COOKIE, ""))
            .path("/")
            .http_only(true)
            .secure(state.config.is_https())
            .same_site(SameSite::Lax)
            .max_age(time::Duration::ZERO)
            .build(),
        _ => return res,
    };
    if let Ok(v) = header::HeaderValue::from_str(&cookie.to_string()) {
        res.headers_mut().append(header::SET_COOKIE, v);
    }
    res
}

/// The navigation with the live "waiting on you" count, for every page.
pub async fn nav(state: &AppState, actor: &Actor, active: &str) -> Nav {
    let mut n = Nav::new(active);
    if actor.is_owner() {
        if let Some(id) = actor.person() {
            let me = state.db.call(move |c| crate::people::get(c, id)).await.ok().flatten();
            n.me = me.map(|p| Me { name: p.name(), email: p.email, icon: p.icon });
        }
        n.waiting = state.db.call(|c| crate::waiting::count(c)).await.unwrap_or(0) as i64;
        let me = actor.person().unwrap_or(0);
        n.unread = state.db.call(move |c| crate::notifications::unread(c, me)).await.unwrap_or(0);
        let now = crate::util::now_ms();
        n.projects = state
            .db
            .call(move |c| {
                Ok(crate::projects::list(c)?
                    .into_iter()
                    .filter(|p| p.status != "archived")
                    .map(|p| NavProject { online: p.agent.online(now), slug: p.slug, name: p.name })
                    .collect())
            })
            .await
            .unwrap_or_default();
        // Outside the projects (their pages are under "/projects"), the bar shows the project
        // last opened, with its tabs, none of them lit.
        let kept = KEPT_PROJECT.try_with(|k| k.clone()).ok().flatten();
        if let Some(slug) = kept.filter(|k| active != "/projects" && n.projects.iter().any(|p| &p.slug == k)) {
            n = n.in_project(state, &slug, "").await;
        }
    }
    n
}

impl Nav {
    pub fn new(active: &str) -> Self {
        Self {
            items: NAV.iter().map(|&(href, name)| NavItem { href, name, active: href == active }).collect(),
            tabs: TABS
                .iter()
                .map(|&(href, name, icon)| Tab { href, name, icon: super::icons::icon(icon), active: href == active })
                .chain(std::iter::once(Tab {
                    href: "/more",
                    name: "More",
                    icon: super::icons::icon("more"),
                    active: active == "/more" || MORE.iter().any(|m| m.0 == active),
                }))
                .collect(),
            back: MORE.iter().any(|m| m.0 == active).then(|| ("/more".to_owned(), "More".to_owned())),
            version: VERSION,
            waiting: 0,
            unread: 0,
            projects: Vec::new(),
            here: None,
            me: None,
        }
    }
}

/// One part of a page's path at the top left (the owner, 2026-11-18, D-222: "it should really be
/// like a path. and going back should be intuitive"): a link up to that level, or, last, where
/// you are.
pub struct PathPart {
    pub label: String,
    pub href: Option<String>,
}

impl PathPart {
    pub fn link(label: impl Into<String>, href: impl Into<String>) -> Self {
        PathPart { label: label.into(), href: Some(href.into()) }
    }

    pub fn here(label: impl Into<String>) -> Self {
        PathPart { label: label.into(), href: None }
    }
}

/// Where the phone's back goes: the level above the page, the last link in its path.
pub fn up(path: &[PathPart]) -> Option<(String, String)> {
    path.iter().rev().find_map(|p| p.href.clone().map(|h| (h, p.label.clone())))
}

impl Nav {
    /// The phone's back goes up one level of the page's path.
    pub fn up(self, path: &[PathPart]) -> Self {
        match up(path) {
            Some((href, label)) => self.back_to(href, label),
            None => self,
        }
    }

    /// The page is the list of projects (or a page under it that isn't one project's).
    pub fn on_projects(&self) -> bool {
        self.items.iter().any(|i| i.href == "/projects" && i.active)
    }

    /// The page belongs to the project `slug`, under its tab `tab` (a key of the project page's
    /// tabs): the desktop bar shows the project and its tabs, with their counts (D-232).
    pub async fn in_project(mut self, state: &AppState, slug: &str, tab: &str) -> Self {
        let s = slug.to_owned();
        let counts = state
            .db
            .call(move |c| {
                let Some(p) = crate::projects::get(c, &s)? else { return Ok(None) };
                let open = |kind: &str| -> anyhow::Result<i64> {
                    Ok(c.query_row(
                        "SELECT count(*) FROM records WHERE project_id = ?1 AND kind = ?2 AND state = 'open'",
                        rusqlite::params![p.id, kind],
                        |r| r.get(0),
                    )?)
                };
                Ok(Some((p.name.clone(), open("question")?, open("pr")?, open("test")?)))
            })
            .await
            .ok()
            .flatten();
        let Some((name, questions, prs, tests)) = counts else { return self };
        let tabs = super::projects::TABS
            .iter()
            .map(|&(key, label)| {
                let (count, warn) = match key {
                    // Questions carries the questions waiting (Q-6, D-250).
                    "questions" => (questions, true),
                    "prs" => (prs, false),
                    // Builds carries the things to try (Q-6).
                    "builds" => (tests, true),
                    _ => (0, false),
                };
                BarTab {
                    label,
                    href: tab_href(slug, key),
                    count: if count > 0 { count.to_string() } else { String::new() },
                    warn,
                    on: key == tab,
                    dlg: key == "settings",
                }
            })
            .collect();
        self.here = Some(Here { slug: slug.to_owned(), name, tabs, tab: tab.to_owned() });
        self
    }

    /// A back bar to `href` on the phone.
    pub fn back_to(mut self, href: impl Into<String>, label: impl Into<String>) -> Self {
        self.back = Some((href.into(), label.into()));
        self
    }
}

pub fn render(t: impl Template) -> AppResult<Response> {
    Ok(Html(t.render()?).into_response())
}

/// The phone's More tab: the sections that don't fit in the tab bar.
#[derive(Template)]
#[template(path = "more.html")]
struct MorePage {
    nav: Nav,
    items: Vec<Tab>,
}

async fn more(State(state): State<AppState>, Extension(actor): Extension<Actor>) -> AppResult<Response> {
    actor.require_owner()?;
    let items = MORE
        .iter()
        .map(|&(href, name, icon)| Tab { href, name, icon: super::icons::icon(icon), active: false })
        .collect();
    render(MorePage { nav: nav(&state, &actor, "/more").await, items })
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
            let (name, person) = pk.finish_login(c, &body.id, &body.credential)?;
            Ok((name, auth::create_session(c, person, "passkey", &limits, ua.as_deref(), Some(&ip2))?))
        })
        .await;
    match result {
        Ok((name, session)) => {
            let e =
                Entry::new(&Actor::Anonymous, Via::Page, Action::Login, format!("signed in with passkey \"{name}\""))
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

pub(super) fn user_agent(headers: &HeaderMap) -> Option<String> {
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
            // A sign-in link names its person; setup, invite and device links have pages of their
            // own (links.rs).
            let tx = c.transaction()?;
            let Some(link) = auth::consume_login_link(&tx, &secret)? else { return Ok(None) };
            let person = link.person.map(|p| crate::people::get(&tx, p)).transpose()?.flatten();
            let Some(person) = person else { return Ok(None) };
            let session = auth::create_session(&tx, person.id, "link", &limits, ua.as_deref(), Some(&ip2))?;
            tx.commit()?;
            Ok(Some((link.purpose, session)))
        })
        .await?;
    let Some((purpose, session)) = session else {
        let e = Entry::new(&Actor::Anonymous, Via::Page, Action::Denied, "sign-in link unknown, used or expired")
            .ip(Some(ip));
        activity::write(&state.db, e).await;
        let page = LoginPage::new(
            None,
            Some("This link has expired or was already used. Run `sudo flow server add-device` on the server for a new one.".into()),
        );
        return Ok((StatusCode::GONE, Html(page.render()?)).into_response());
    };
    let e = Entry::new(&Actor::Anonymous, Via::Page, Action::Login, "signed in with a one-time link").ip(Some(ip));
    activity::write(&state.db, e).await;
    let jar = jar.add(session_cookie(&state, session));
    // A link from before people (`flow server add-passkey`) led straight to adding a passkey.
    let to = if purpose == "add-passkey" { "/settings?add_passkey=1#passkeys" } else { "/" };
    Ok((jar, Redirect::to(to)).into_response())
}

pub(super) fn session_cookie(state: &AppState, value: String) -> Cookie<'static> {
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
