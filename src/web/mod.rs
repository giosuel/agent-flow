//! The HTTP server: the owner's pages, the JSON API, and the MCP endpoint, all behind one
//! authentication step.

pub(crate) mod admin_socket;
mod api;
mod approvals;
mod assets;
pub(crate) mod builders;
mod builds;
pub(crate) mod consultant;
pub mod outgoing;
pub mod remote;
mod screenshots;
mod sync;
mod totest;
pub use builds::notify_build;
mod buildstab;
mod buttons;
mod charts;
mod conversations;
mod dashboard;
mod decisions;
mod editor;
mod handbook;
mod icons;
mod log;
mod machine;
mod new_project;
mod notes;
mod notifications;
mod overview;
mod pages;
mod projectdoc;
mod projects;
mod pwa;
mod settings;
mod steps;
mod tone;
mod uilib;
mod usage;

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
    /// Posts the owner's sent drafts to other people's repositories.
    pub poster: Arc<dyn crate::outgoing::Poster>,
    /// The build servers as last seen, and their running jobs.
    pub remote: crate::remote::Shared,
    /// Denied requests logged per client address in the current minute (flood guard).
    denied: Arc<std::sync::Mutex<std::collections::HashMap<String, (i64, u32)>>>,
    /// Running `/api/wait` calls per project.
    pub waits: Arc<std::sync::Mutex<std::collections::HashMap<i64, u32>>>,
    /// Held while a build uploads: one at a time.
    pub uploads: Arc<tokio::sync::Mutex<()>>,
    /// Tokens already reported as used from two places, with the day: one alert a day each.
    two_places: Arc<std::sync::Mutex<std::collections::HashMap<i64, i64>>>,
}

impl AppState {
    pub fn new(db: Db, config: Config, vapid: Vapid, github: Arc<dyn GitHub>) -> Self {
        let passkeys = Passkeys::new(&config);
        let push = Pusher::new(db.clone(), vapid, &config.push_contact);
        let poster = Arc::new(crate::outgoing::GhPoster { token_file: config.external_token_file.clone() });
        Self {
            db,
            config: Arc::new(config),
            passkeys,
            push,
            github,
            poster,
            remote: Arc::default(),
            denied: Arc::default(),
            waits: Arc::default(),
            uploads: Arc::default(),
            two_places: Arc::default(),
        }
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

    /// At most 20 logged denials per address and minute; the rest only count as noise. An
    /// address's anonymous requests to the public paths (the sign-in page, setup codes, approval
    /// polls) get the same allowance of their own, or a loop would fill the Log a row at a time.
    pub(crate) fn log_denial(&self, ip: &str) -> bool {
        let minute = crate::util::now_ms() / 60_000;
        let mut map = crate::util::lock(&self.denied);
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
    let app = Router::new()
        .merge(pages::routes())
        .merge(settings::routes())
        .merge(projects::routes())
        .merge(api::routes())
        .merge(approvals::routes())
        .merge(sync::routes())
        .merge(builders::routes())
        .merge(builders::admin_routes())
        .merge(consultant::routes())
        .merge(notifications::routes())
        .merge(buttons::routes())
        .merge(builds::routes())
        .merge(screenshots::routes())
        .merge(notes::routes())
        .merge(outgoing::routes())
        .merge(remote::routes())
        .merge(totest::routes())
        .merge(steps::routes())
        .merge(projectdoc::routes())
        .merge(conversations::routes())
        .merge(handbook::routes())
        .merge(log::routes())
        .merge(machine::routes())
        .merge(new_project::routes())
        .merge(usage::routes())
        .merge(uilib::routes())
        .merge(dashboard::routes())
        .merge(pwa::routes())
        .nest_service("/mcp", mcp)
        .merge(assets::routes())
        .fallback(|| async { pages::error_page(StatusCode::NOT_FOUND, "There's no page here.") })
        .layer(middleware::from_fn_with_state(state.clone(), pages::keep_project))
        .layer(middleware::from_fn_with_state(state.clone(), log_requests))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .layer(middleware::from_fn(security_headers))
        .with_state(state);
    // Before routing (a Router's own layers run after it): a dialog's address opened directly is
    // routed to the page it opens over.
    Router::new().fallback_service(app).layer(middleware::from_fn(dialogs_over_pages))
}

/// What opens as a dialog, and over which page: (the page, the dialog's kind). Records (the record
/// dialog), Settings (over the dashboard), and Add project (over Projects).
fn dialog_parent(uri: &axum::http::Uri) -> Option<(String, &'static str)> {
    let path = uri.path();
    if path == "/settings" {
        return Some(("/".into(), "settings"));
    }
    if path == "/notifications" {
        return Some(("/".into(), "ntf"));
    }
    if uri.query().is_some() {
        return None;
    }
    if path == "/projects/new" {
        return Some(("/projects".into(), "dlg"));
    }
    let parts: Vec<&str> = path.split('/').collect();
    let record = |s: &str| {
        s.split_once('-').is_some_and(|(k, n)| {
            ["T", "Q", "D", "ADR", "PR", "TT", "S", "F", "N"].contains(&k)
                && !n.is_empty()
                && n.bytes().all(|b| b.is_ascii_digit())
        })
    };
    match parts.as_slice() {
        ["", "p", slug, r] if record(r) => Some((format!("/p/{slug}"), "rec")),
        ["", "p", slug, "doc" | "settings"] => Some((format!("/p/{slug}"), "dlg")),
        ["", "p", slug, "steps", n] if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => {
            Some((format!("/p/{slug}"), "rec"))
        }
        _ => None,
    }
}

/// Dialogs are dialogs wherever they're opened from (the owner, 2026-11-17: "Notes are not
/// consistent. the link opens a page while clicking on one opens a dialog. how can we unify this? i
/// also saw this with settings earlier"). Opened directly (a notification, the bell in a new tab, a
/// reload), the address shows the page it belongs to with the dialog open over it; scripts still
/// fetch the dialog's own page (they don't navigate).
async fn dialogs_over_pages(req: axum::extract::Request, next: Next) -> Response {
    // A browser going to the address: Sec-Fetch-Mode says so over https; over plain http (the dev
    // instance) browsers leave it out but still send Upgrade-Insecure-Requests on navigations.
    // Scripts' fetches and other clients send neither.
    let h = req.headers();
    let navigate = req.method() == axum::http::Method::GET
        && match h.get("sec-fetch-mode") {
            Some(m) => m.as_bytes() == b"navigate",
            None => h.get("upgrade-insecure-requests").is_some_and(|v| v.as_bytes() == b"1"),
        };
    let Some((parent, kind)) = (if navigate { dialog_parent(req.uri()) } else { None }) else {
        return next.run(req).await;
    };
    let target = req.uri().path_and_query().map(|p| p.as_str().to_owned()).unwrap_or_default();
    let mut req = req;
    match parent.parse() {
        Ok(u) => *req.uri_mut() = u,
        Err(_) => return next.run(req).await,
    }
    let res = next.run(req).await;
    let html = res
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .is_some_and(|v| v.to_str().unwrap_or("").starts_with("text/html"));
    if !res.status().is_success() || !html {
        return res;
    }
    let (mut parts, body) = res.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 32 << 20).await else {
        return pages::error_page(StatusCode::INTERNAL_SERVER_ERROR, "The page couldn't be read.");
    };
    let esc = |s: &str| s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;");
    let page = String::from_utf8_lossy(&bytes).replacen(
        "<body",
        &format!(r#"<body data-open-dlg="{kind}" data-open-url="{}" data-open-from="{}""#, esc(&target), esc(&parent)),
        1,
    );
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    Response::from_parts(parts, axum::body::Body::from(page))
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
    // The handbook: brought in once from the old ai-handbook checkout, then written out as files.
    let (data, from) = (state.config.data_dir(), state.config.handbook_dir());
    match state
        .db
        .call(move |c| {
            let n = crate::handbook::import(c, &from)?;
            crate::handbook::mirror(c, &data)?;
            Ok(n)
        })
        .await
    {
        Ok(0) => {}
        Ok(n) => tracing::info!("handbook: {n} files brought in from the ai-handbook folder"),
        Err(e) => tracing::warn!("handbook: {e:#}"),
    }
    // Subagent runs from before their model was read get it from the usage records (D-228).
    match state.db.call(|c| crate::sessionlog::backfill_models(c)).await {
        Ok(0) => {}
        Ok(n) => tracing::info!("session log: the model of {n} earlier subagent run(s) filled in"),
        Err(e) => tracing::warn!("session log: filling in models failed: {e:#}"),
    }
    // Sessions a stray title had named "\\" get their name back (see crate::sessionlog::is_name).
    match state.db.call(|c| crate::sessionlog::repair_names(c)).await {
        Ok(0) => {}
        Ok(n) => tracing::info!("session log: {n} session name(s) repaired"),
        Err(e) => tracing::warn!("session log: repairing names failed: {e:#}"),
    }
    if recovered > 0 {
        tracing::warn!("{recovered} button(s) were running when AgentFlow stopped; marked failed");
    }
    let swept = crate::builds::sweep_incoming(&crate::builds::dir(&state.config.data_dir()));
    if swept > 0 {
        tracing::info!("removed {swept} unfinished build upload(s)");
    }
    {
        let data = state.config.data_dir();
        let _ = state
            .db
            .call(move |c| {
                crate::secrets::load_all(c, &data);
                Ok(())
            })
            .await;
    }
    crate::poller::spawn(state.clone());
    if state.config.local_transcripts {
        spawn_usage(state.clone());
    } else {
        tracing::info!("conversations come by flow sync (local_transcripts is off)");
    }
    if !state.config.build_servers.is_empty() {
        tracing::warn!(
            "config: build_servers (SSH builders) are no longer used; remove them. Builders connect themselves: flow builder add"
        );
    }
    crate::backup::spawn(state.clone());
    crate::machine::spawn_sampler(state.clone());
    crate::remote::spawn(state.clone());
    spawn_archiver(state.clone());
    admin_socket::spawn(state.clone());
    consultant::spawn_watchdog(state.clone());
    admin_socket::first_start_link(&state).await;
    let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
    // On stop, the requests under way get a few seconds to finish. Streams that never end on their
    // own (the Log's live view, a builder's connection, MCP sessions) would otherwise hold the old
    // server until systemd kills it after 90 s, and AgentFlow is down meanwhile (2026-11-23).
    let (stopping, stopped) = tokio::sync::oneshot::channel::<()>();
    let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
        shutdown_signal().await;
        let _ = stopping.send(());
    });
    tokio::select! {
        r = serve => r?,
        () = async {
            if stopped.await.is_err() {
                std::future::pending::<()>().await;
            }
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } => tracing::info!("stopping: closed the connections still open after {} s", SHUTDOWN_GRACE.as_secs()),
    }
    Ok(())
}

/// How long the requests under way may take to finish once the server is told to stop.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Wakes the transcript reader now (the Reload button on Usage and Conversations).
pub static READ_NOW: tokio::sync::Notify = tokio::sync::Notify::const_new();
/// The reader is going through the transcripts right now.
pub static READING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// When transcripts last came in (read here, or sent by `flow sync`), in ms; 0 since the start.
pub static LAST_READ: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// Reads new token usage from the transcripts: at start, then every five minutes. Files are read
/// outside the database's lock, one at a time, so a first read of gigabytes doesn't hold up pages.
fn spawn_usage(state: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(300));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = READ_NOW.notified() => {}
            }
            let started = std::time::Instant::now();
            READING.store(true, std::sync::atomic::Ordering::Relaxed);
            let result = usage_round(&state).await;
            READING.store(false, std::sync::atomic::Ordering::Relaxed);
            LAST_READ.store(crate::util::now_ms(), std::sync::atomic::Ordering::Relaxed);
            match result {
                Ok(s) if s.calls > 0 => tracing::info!(
                    "token usage: {} new calls from {} transcripts in {:.1} s",
                    s.calls,
                    s.files,
                    started.elapsed().as_secs_f64()
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!("token usage: {e:#}"),
            }
        }
    });
}

async fn usage_round(state: &AppState) -> anyhow::Result<crate::usage::Stats> {
    use crate::hands::{Job, run_async};
    // The transcripts are the agents' user's own files: read as that user (see crate::hands).
    let root = crate::usage::root(&state.config);
    let files: Vec<std::path::PathBuf> = run_async(Job::Transcripts { root: root.clone() }).await?;
    let mut stats = crate::usage::Stats::default();
    for path in files {
        let p = path.clone();
        let (offset, first_at) = state.db.call(move |c| crate::usage::file_state(c, &p)).await?;
        let shown = path.display().to_string();
        let job = Job::ReadTranscript { root: root.clone(), path, offset, first_at };
        let read: Option<crate::usage::Read> = match run_async(job).await {
            Ok(read) => read,
            // Deleted meanwhile (Claude Code cleans up old ones) is expected; anything else is
            // worth a warning. Either way the rest go on.
            Err(e) => {
                if e.downcast_ref::<crate::hands::Coded>().is_some_and(|c| c.code == "gone") {
                    tracing::debug!("token usage: {shown}: {e:#}");
                } else {
                    tracing::warn!("token usage: {shown}: {e:#}");
                }
                continue;
            }
        };
        if let Some(read) = read {
            let n = state.db.call(move |c| crate::usage::store(c, &read)).await?;
            if n > 0 {
                stats.files += 1;
                stats.calls += n;
            }
        }
    }
    state.db.call(|c| crate::usage::assign_projects(c)).await?;
    Ok(stats)
}

/// Moves reads older than 90 days to the monthly archive and cleans up builders' old files: at
/// start, then every six hours.
fn spawn_archiver(state: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(6 * 3600));
        loop {
            tick.tick().await;
            let data = state.config.data_dir();
            let now = crate::util::now_ms();
            let _ = state.db.call(|c| crate::notifications::purge(c)).await;
            match state.db.call(move |c| crate::logview::archive_old_reads(c, &data, now)).await {
                Ok(0) => {}
                Ok(n) => tracing::info!("archived {n} reads older than 90 days"),
                Err(e) => tracing::warn!("archiving old reads: {e:#}"),
            }
            // Builders' files: blobs unused for two weeks, jobs' results after a week.
            let data = state.config.data_dir();
            let freed = tokio::task::spawn_blocking(move || {
                let (n, bytes) = crate::blobs::Store::new(data.join("blobs")).gc(DAY * 14);
                let results = clean_old(&data.join("remote").join("results"), DAY * 7);
                (n, bytes, results)
            })
            .await;
            if let Ok((n, bytes, results)) = freed
                && (n > 0 || results > 0)
            {
                tracing::info!("removed {n} unused blobs ({} MB) and {results} old jobs' results", bytes >> 20);
            }
        }
    });
}

const DAY: std::time::Duration = std::time::Duration::from_secs(86400);

/// Removes the folders in `dir` last changed more than `age` ago. Returns how many.
fn clean_old(dir: &std::path::Path, age: std::time::Duration) -> usize {
    let cutoff = std::time::SystemTime::now() - age;
    let Ok(rd) = std::fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .filter(|e| e.metadata().is_ok_and(|m| m.is_dir() && m.modified().is_ok_and(|t| t < cutoff)))
        .filter(|e| std::fs::remove_dir_all(e.path()).is_ok())
        .count()
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
    path == "/login"
        || path.starts_with("/login/")
        || assets::is_asset(path)
        || pwa::is_public(path)
        || approvals::is_public(path)
}

/// Pages and API calls that don't get a row of their own in the activity log. MCP tool calls log
/// themselves, with the tool name and record, which says more than "POST /mcp".
fn is_unlogged(path: &str) -> bool {
    assets::is_asset(path)
        || pwa::is_public(path)
        || path == "/mcp"
        || path.starts_with("/mcp/")
        // The Log reading itself (scrolling, the live stream) would fill the Log.
        || path == "/log/rows"
        || path == "/log/stream"
        // flow sync after every reply: its rounds are logged by the handler, once and only when
        // something new came (the owner, 2026-11-17: "looks like spam again").
        || path == "/api/sync/state"
        || path == "/api/sync"
        // A job's output, asked for every second by each waiting `flow remote` and the job's page:
        // the job is in the Log already, with its outcome and lines (the owner, 2026-11-17).
        || (path.ends_with("/output") && (path.starts_with("/api/remote/jobs/") || path.starts_with("/machine/jobs/")))
}

fn client_ip(config: &Config, peer: Option<IpAddr>, headers: &HeaderMap) -> String {
    let Some(peer) = peer else { return "unknown".into() };
    // The peer's address says only "this machine" when the proxy is local: with a secret set,
    // the proxy has to show it too, or a local process could name any address it likes.
    let vouched = config.proxy_secret.is_empty()
        || headers
            .get("x-agentflow-proxy")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| crate::util::hash_secret(v) == crate::util::hash_secret(&config.proxy_secret));
    if config.trusted_proxies.contains(&peer)
        && vouched
        && let Some(forwarded) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok())
        && let Some(last) = forwarded.rsplit(',').next()
    {
        return last.trim().to_owned();
    }
    peer.to_string()
}

/// A token used from two addresses within ten minutes: a warning in the log and an urgent
/// notification, once a day per token (a token that keeps alternating would otherwise write one
/// on every request).
async fn two_places(state: &AppState, actor: &Actor, other: &str, here: &str) {
    let Some(id) = actor.token_id() else { return };
    let day = crate::util::now_ms() / 86_400_000;
    if crate::util::lock(&state.two_places).insert(id, day) == Some(day) {
        return;
    }
    let what = format!(
        "{}'s token was used from {other} and {here} within ten minutes; if one of them isn't its agent, rotate it in Settings",
        actor.name()
    );
    let e = Entry::new(actor, Via::Api, Action::System, what.clone()).severity("warn").ip(Some(here.to_owned()));
    activity::write(&state.db, e).await;
    state.push.spawn(
        crate::push::Kind::Urgent,
        crate::push::Notification {
            title: "A token is in two places".into(),
            body: what,
            url: "/settings".into(),
            tag: Some(format!("two-places-{id}")),
        },
    );
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
            (Some(token), _) => auth::token_lookup(c, &token, &from).map(|a| match a {
                Some((actor, elsewhere)) => (Some(actor), true, elsewhere),
                None => (None, true, None),
            }),
            (None, Some(secret)) => auth::session_actor(c, &secret, &limits).map(|a| (a, false, None)),
            (None, None) => Ok((None, false, None)),
        })
        .await;
    let (actor, tried_token) = match looked_up {
        Ok((actor, tried, elsewhere)) => {
            if let (Some(a), Some(other)) = (&actor, elsewhere) {
                two_places(&state, a, &other, &ip).await;
            }
            (actor, tried)
        }
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

    // A builder's token opens the builder's own endpoints and nothing else.
    if matches!(actor, Actor::Builder { .. }) && !path.starts_with("/api/builders/") {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"error": "a builder's token is only for /api/builders"})),
        )
            .into_response();
    }

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
        // Back to the page after signing in, e.g. an approval link opened on the phone.
        let plain = path.chars().all(|c| c.is_ascii_alphanumeric() || "/-_.".contains(c));
        return if plain && path != "/" && !path.starts_with("//") {
            Redirect::to(&format!("/login?next={path}")).into_response()
        } else {
            Redirect::to("/login").into_response()
        };
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
    // Nor is a loop over the public paths: an address's first rows a minute say enough.
    if matches!(actor, Actor::Anonymous) && !state.log_denial(&format!("anonymous {}", ip.as_deref().unwrap_or(""))) {
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
