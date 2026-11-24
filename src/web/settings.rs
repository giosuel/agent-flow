//! Settings: passkeys, notifications, tokens and signed-in browsers. Owner only.

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Form, Json, Router};
use rusqlite::params;
use serde::Deserialize;
use serde_json::json;
use webauthn_rs::prelude::RegisterPublicKeyCredential;

use super::pages::{Nav, nav, render};
use super::{AppError, AppResult, AppState, ClientIp, json_error};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::{self, Actor};
use crate::push::{self, Kind, Notification};
use crate::util::{ago, now_ms};
use crate::{passkey, tokens};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/settings", get(page))
        .route("/settings/passkeys/start", post(passkey_start))
        .route("/settings/passkeys/finish", post(passkey_finish))
        .route("/settings/passkeys/{id}/remove", post(passkey_remove))
        .route("/settings/push/subscribe", post(push_subscribe))
        .route("/settings/push/{id}/remove", post(push_remove))
        .route("/settings/push/test", post(push_test))
        .route("/settings/notify", post(notify_save))
        .route("/settings/github", post(github_save))
        .route("/settings/github/remove", post(github_remove))
        .route("/settings/tokens/{id}/revoke", post(token_revoke))
        .route("/settings/tokens/{id}/rotate", post(token_rotate))
        .route("/settings/sessions/{id}/end", post(session_end))
}

/// The refusal for a non-owner calling one of the JSON endpoints, if it isn't the owner.
fn not_owner(actor: &Actor) -> Option<Response> {
    actor.require_owner().err().map(|f| json_error(StatusCode::FORBIDDEN, f.0))
}

async fn log(state: &AppState, actor: &Actor, ip: &ClientIp, action: Action, summary: String) {
    activity::write(&state.db, Entry::new(actor, Via::Page, action, summary).ip(Some(ip.0.clone()))).await;
}

// ---------------------------------------------------------------------------------------------
// The page

struct PasskeyView {
    id: i64,
    name: String,
    added: String,
    used: String,
}

struct DeviceView {
    id: i64,
    device: String,
    added: String,
    status: String,
    failing: bool,
}

struct ToggleView {
    key: &'static str,
    label: &'static str,
    on: bool,
}

struct TokenView {
    id: i64,
    holder: String,
    kind: String,
    can: &'static str,
    used: String,
    added: String,
    from: String,
    /// "…a1b2", the token's end.
    hint: String,
}

struct SessionView {
    id: i64,
    how: String,
    device: String,
    seen: String,
    current: bool,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    nav: Nav,
    notice: Option<&'static str>,
    /// The server's GitHub tokens: (which, label, what it's for, set as "…abcd · login").
    github: Vec<(&'static str, &'static str, &'static str, Option<String>)>,
    /// Requests from `flow` waiting for approval: (code, what, machine).
    approvals: Vec<(String, String, String)>,
    passkeys_available: bool,
    public_url: String,
    /// This server's glibc: the downloaded program needs one at least as new.
    glibc: String,
    passkeys: Vec<PasskeyView>,
    push_key: String,
    push_devices: Vec<DeviceView>,
    toggles: Vec<ToggleView>,
    device_tokens: Vec<TokenView>,
    agent_tokens: Vec<TokenView>,
    sessions: Vec<SessionView>,
    /// The Build servers card.
    build_servers: String,
}

#[derive(Deserialize)]
struct PageQuery {
    done: Option<String>,
    /// What happened on the Build servers card.
    bs: Option<String>,
    /// Signed in with a link from `flow server add-passkey`: add one now.
    add_passkey: Option<String>,
}

/// Messages after a form post; the query names one of these, never free text.
fn notice_for(key: &str) -> Option<&'static str> {
    Some(match key {
        "passkey-removed" => "Passkey removed.",
        "device-removed" => "That device won't get notifications any more.",
        "test-sent" => "Test notification sent to every device that has notifications on.",
        "test-none" => "No device has notifications on yet.",
        "notify-saved" => "Notification settings saved.",
        "revoked" => "Token revoked. Whatever used it can no longer reach AgentFlow.",
        "rotated" => "New token written to its file; the old one is revoked. The agent picks it up when it reconnects.",
        "session-ended" => "That browser is signed out.",
        "github-saved" => "GitHub token saved. AgentFlow's gh calls use it from now on.",
        "github-removed" => "GitHub token removed. gh uses the login of the user it runs as again.",
        _ => return None,
    })
}

async fn page(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<PageQuery>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let notice = if q.add_passkey.is_some() {
        Some("You're signed in with a one-time link. Add a passkey for this device below, so you can sign in again.")
    } else {
        q.done.as_deref().and_then(notice_for)
    };
    render_page(&state, &actor, notice, q.bs.as_deref()).await
}

async fn render_page(
    state: &AppState,
    actor: &Actor,
    notice: Option<&'static str>,
    bs: Option<&str>,
) -> AppResult<Response> {
    let current = match actor {
        Actor::Owner { session_id, .. } => *session_id,
        _ => 0,
    };
    let now = now_ms();
    let (passkeys, devices, toggles, tokens, sessions) = state
        .db
        .call(move |c| {
            let passkeys = passkey::list(c)?;
            let devices = push::devices(c)?;
            let toggles =
                Kind::ALL.iter().map(|&k| Ok((k, push::is_enabled(c, k)?))).collect::<anyhow::Result<Vec<_>>>()?;
            let tokens = auth::list_tokens(c)?;
            let mut stmt = c.prepare(
                "SELECT id, method, user_agent, last_seen_at FROM web_sessions
                 WHERE revoked_at IS NULL AND expires_at > ?1 ORDER BY last_seen_at DESC",
            )?;
            let sessions = stmt
                .query_map([now], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok((passkeys, devices, toggles, tokens, sessions))
        })
        .await?;

    let when = |t: Option<i64>| t.map(|t| ago(t, now)).unwrap_or_else(|| "never".into());
    let active = tokens.into_iter().filter(|t| t.revoked_at.is_none());
    let (device_tokens, agent_tokens): (Vec<_>, Vec<_>) = active
        .map(|t| TokenView {
            id: t.id,
            holder: match &t.project {
                Some(p) => p.clone(),
                None => t.name.clone(),
            },
            can: match t.kind.as_str() {
                "project" => "write own, read all",
                "admin" => "admin",
                _ => "read, CLI and viber-run",
            },
            used: when(t.last_used_at),
            added: ago(t.created_at, now),
            from: t.last_used_from.clone().unwrap_or_default(),
            hint: if t.hint.is_empty() { String::new() } else { format!("…{}", t.hint) },
            kind: t.kind,
        })
        .partition(|t| t.kind == "device");

    let page = SettingsPage {
        nav: nav(state, actor, "/settings").await,
        notice,
        github: {
            use crate::secrets::{GitHub, token};
            let login = |k: &'static str| async move {
                state.db.call(move |c| Ok(c.query_row("SELECT value FROM settings WHERE key = ?1", [k], |r| r.get::<_, String>(0)).ok())).await.ok().flatten()
            };
            let show = |t: Option<String>, l: Option<String>| t.map(|t| format!("…{}{}", crate::secrets::hint(&t), l.map(|l| format!(" · {l}")).unwrap_or_default()));
            vec![
                ("own", "Your repositories", "merging, tags, reading PRs", show(token(GitHub::Own), login("github.login.own").await)),
                ("external", "Other people's repositories", "sending the contributions you approve", show(token(GitHub::External), login("github.login.external").await)),
            ]
        },
        approvals: state
            .db
            .call(|c| crate::approvals::pending(c))
            .await?
            .iter()
            .map(|a| (a.code.clone(), crate::approvals::describe(a), a.machine.clone()))
            .collect(),
        build_servers: super::remote::servers_card(state, bs).await?,
        passkeys_available: state.passkeys.available(),
        public_url: state.config.public_url.clone(),
        glibc: glibc_version(),
        passkeys: passkeys
            .into_iter()
            .map(|p| PasskeyView { id: p.id, name: p.name, added: ago(p.created_at, now), used: when(p.last_used_at) })
            .collect(),
        push_key: state.push.public_key(),
        push_devices: devices
            .into_iter()
            .map(|d| DeviceView {
                id: d.id,
                device: d.device,
                added: ago(d.created_at, now),
                failing: d.last_error.is_some(),
                status: match (&d.last_error, d.last_ok_at) {
                    (Some(e), _) => format!("last push failed: {e}"),
                    (None, Some(t)) => format!("last push {}", ago(t, now)),
                    (None, None) => "no push yet".into(),
                },
            })
            .collect(),
        toggles: toggles.into_iter().map(|(k, on)| ToggleView { key: k.key(), label: k.label(), on }).collect(),
        device_tokens,
        agent_tokens,
        sessions: sessions
            .into_iter()
            .map(|(id, method, ua, seen)| SessionView {
                id,
                how: method,
                device: ua.as_deref().map(describe_agent).unwrap_or_else(|| "unknown browser".into()),
                seen: ago(seen, now),
                current: id == current,
            })
            .collect(),
    };
    render(page)
}

/// "Safari on iPhone" from a User-Agent header, for the list of signed-in browsers.
fn describe_agent(ua: &str) -> String {
    let device = ["iPhone", "iPad", "Android", "Macintosh", "Windows", "Linux"].into_iter().find(|d| ua.contains(d));
    let browser = if ua.contains("Firefox/") {
        "Firefox"
    } else if ua.contains("Edg/") {
        "Edge"
    } else if ua.contains("Chrome/") {
        "Chrome"
    } else if ua.contains("Safari/") {
        "Safari"
    } else {
        "Browser"
    };
    match device {
        Some("Macintosh") => format!("{browser} on macOS"),
        Some(d) => format!("{browser} on {d}"),
        None => browser.into(),
    }
}

// ---------------------------------------------------------------------------------------------
// Passkeys

async fn passkey_start(State(state): State<AppState>, Extension(actor): Extension<Actor>) -> Response {
    if let Some(r) = not_owner(&actor) {
        return r;
    }
    let pk = state.passkeys.clone();
    match state.db.call(move |c| pk.start_registration(c)).await {
        Ok((id, options)) => Json(json!({"id": id, "options": options})).into_response(),
        Err(e) => json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct PasskeyFinish {
    id: String,
    name: String,
    credential: RegisterPublicKeyCredential,
}

async fn passkey_finish(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Json(body): Json<PasskeyFinish>,
) -> Response {
    if let Some(r) = not_owner(&actor) {
        return r;
    }
    let pk = state.passkeys.clone();
    let name = body.name.clone();
    match state.db.call(move |c| pk.finish_registration(c, &body.id, &body.name, &body.credential)).await {
        Ok(id) => {
            log(&state, &actor, &ip, Action::Write, format!("passkey {id} added: {name}")).await;
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

async fn passkey_remove(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Path(id): Path<i64>,
) -> AppResult<Redirect> {
    actor.require_owner()?;
    if !state.db.call(move |c| passkey::remove(c, id)).await? {
        return Err(AppError::NotFound);
    }
    log(&state, &actor, &ip, Action::Write, format!("passkey {id} removed")).await;
    Ok(Redirect::to("/settings?done=passkey-removed"))
}

// ---------------------------------------------------------------------------------------------
// Notifications

#[derive(Deserialize)]
struct Subscribe {
    subscription: push::Subscription,
    device: String,
}

async fn push_subscribe(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Json(body): Json<Subscribe>,
) -> Response {
    if let Some(r) = not_owner(&actor) {
        return r;
    }
    let device: String = body.device.trim().chars().take(60).collect();
    let device = if device.is_empty() { "Browser".to_owned() } else { device };
    let name = device.clone();
    match state.db.call(move |c| push::save_subscription(c, &body.subscription, &device)).await {
        Ok(()) => {
            log(&state, &actor, &ip, Action::Write, format!("notifications turned on for {name}")).await;
            Json(json!({"ok": true})).into_response()
        }
        Err(e) => json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

async fn push_remove(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Path(id): Path<i64>,
) -> AppResult<Redirect> {
    actor.require_owner()?;
    if !state.db.call(move |c| push::remove_device(c, id)).await? {
        return Err(AppError::NotFound);
    }
    log(&state, &actor, &ip, Action::Write, format!("notifications turned off for device {id}")).await;
    Ok(Redirect::to("/settings?done=device-removed"))
}

async fn push_test(State(state): State<AppState>, Extension(actor): Extension<Actor>) -> AppResult<Redirect> {
    actor.require_owner()?;
    let n = Notification {
        title: "AgentFlow".into(),
        body: "Notifications work on this device.".into(),
        url: "/settings".into(),
        tag: Some("test".into()),
    };
    // A test goes out even if "urgent" is switched off: it's sent as its own kind of check.
    let delivered = state.push.send_ignoring_settings(&n).await?;
    Ok(Redirect::to(if delivered > 0 { "/settings?done=test-sent" } else { "/settings?done=test-none" }))
}

#[derive(Deserialize)]
struct GitHubForm {
    which: String,
    #[serde(default)]
    token: String,
}

/// Checks a GitHub token by asking GitHub whose it is, then keeps it sealed (see crate::secrets).
async fn github_save(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Form(f): Form<GitHubForm>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let which = crate::secrets::GitHub::parse(&f.which).map_err(|e| AppError::BadRequest(format!("{e:#}")))?;
    let token = f.token.trim().to_owned();
    crate::secrets::check_token(&token).map_err(|e| AppError::BadRequest(format!("{e:#}")))?;
    let login = github_login(&token).await.map_err(|e| AppError::BadRequest(format!("GitHub didn't take that token: {e:#}")))?;
    let data = state.config.data_dir();
    let (t, l, w) = (token.clone(), login.clone(), f.which.clone());
    state
        .db
        .call(move |c| {
            crate::secrets::store(c, &data, which, &t)?;
            c.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                rusqlite::params![format!("github.login.{w}"), l],
            )?;
            Ok(())
        })
        .await?;
    log(&state, &actor, &ip, Action::Write, format!("GitHub token for {} set (account {login})", f.which)).await;
    Ok(Redirect::to("/settings?done=github-saved#s-github").into_response())
}

async fn github_remove(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Form(f): Form<GitHubForm>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let which = crate::secrets::GitHub::parse(&f.which).map_err(|e| AppError::BadRequest(format!("{e:#}")))?;
    state.db.call(move |c| crate::secrets::remove(c, which)).await?;
    log(&state, &actor, &ip, Action::Write, format!("GitHub token for {} removed", f.which)).await;
    Ok(Redirect::to("/settings?done=github-removed#s-github").into_response())
}

/// The account a token belongs to.
async fn github_login(token: &str) -> anyhow::Result<String> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        tokio::process::Command::new("gh")
            .args(["api", "user", "--jq", ".login"])
            .env("GH_TOKEN", token)
            .env("GH_PROMPT_DISABLED", "1")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("GitHub didn't answer within 30 seconds"))??;
    if !out.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).replace(token, "…").trim().lines().last().unwrap_or("refused"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

async fn notify_save(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Form(form): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let on: Vec<Kind> = form.iter().filter_map(|(k, _)| k.strip_prefix("notify.").and_then(Kind::from_key)).collect();
    let summary = format!(
        "notifications: {}",
        Kind::ALL
            .iter()
            .map(|k| format!("{} {}", k.key(), if on.contains(k) { "on" } else { "off" }))
            .collect::<Vec<_>>()
            .join(", ")
    );
    state
        .db
        .call(move |c| {
            for k in Kind::ALL {
                push::set_enabled(c, k, on.contains(&k))?;
            }
            Ok(())
        })
        .await?;
    log(&state, &actor, &ip, Action::Write, summary).await;
    Ok(Redirect::to("/settings?done=notify-saved").into_response())
}

// ---------------------------------------------------------------------------------------------
// Tokens and sessions

async fn token_revoke(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Path(id): Path<i64>,
) -> AppResult<Redirect> {
    actor.require_owner()?;
    if !state.db.call(move |c| auth::revoke_token(c, id)).await? {
        return Err(AppError::NotFound);
    }
    log(&state, &actor, &ip, Action::Write, format!("token {id} revoked")).await;
    Ok(Redirect::to("/settings?done=revoked"))
}

async fn token_rotate(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Path(id): Path<i64>,
) -> AppResult<Redirect> {
    actor.require_owner()?;
    let dir = state.config.token_dir();
    let (new_id, path) = state.db.call(move |c| tokens::rotate(c, id, &dir)).await?;
    log(&state, &actor, &ip, Action::Write, format!("token {id} rotated to {new_id}, written to {}", path.display()))
        .await;
    Ok(Redirect::to("/settings?done=rotated"))
}

async fn session_end(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Path(id): Path<i64>,
) -> AppResult<Redirect> {
    actor.require_owner()?;
    state
        .db
        .call(move |c| {
            c.execute(
                "UPDATE web_sessions SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
                params![now_ms(), id],
            )?;
            Ok(())
        })
        .await?;
    log(&state, &actor, &ip, Action::Login, format!("browser session {id} signed out")).await;
    Ok(Redirect::to("/settings?done=session-ended"))
}

/// "2.43", from `ldd --version`; empty if it can't be told.
pub(crate) fn glibc_version() -> String {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        let out = std::process::Command::new("ldd").arg("--version").output();
        out.ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).lines().next().map(str::to_owned))
            .and_then(|l| l.split_whitespace().last().map(str::to_owned))
            .unwrap_or_default()
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agents() {
        let iphone = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1";
        assert_eq!(describe_agent(iphone), "Safari on iPhone");
        let firefox = "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0";
        assert_eq!(describe_agent(firefox), "Firefox on Linux");
    }
}
