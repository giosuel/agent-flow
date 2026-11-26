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
        .route("/settings/passkeys/link", post(device_link))
        .route("/settings/people/invite", post(invite))
        .route("/settings/people/{id}/link", post(person_link))
        .route("/settings/people/{id}/remove", post(person_remove))
        .route("/settings/links/{id}/withdraw", post(link_withdraw))
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
    /// Rotate is offered only where it works (see `tokens::rotates_here`).
    rotate: bool,
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
    /// Your username, email and icon (Settings › Profile).
    profile: (String, String, String),
    /// Everyone on this AgentFlow, the admin first (Settings › People).
    people: Vec<PersonView>,
    /// You're the admin: you invite people.
    admin: bool,
    admin_name: String,
    /// Invites not used yet: (row, made, works until).
    invites: Vec<(i64, String, String)>,
    /// A link just made, shown once.
    made: Option<Made>,
    /// The sections, in groups, for the list on the left.
    groups: Vec<(&'static str, Vec<SectionLink>)>,
    /// The one shown.
    section: &'static str,
    /// Picked explicitly (on the phone: the section's own screen instead of the list).
    chosen: bool,
    /// The server's GitHub tokens: (which, label, what it's for, set as "…abcd · login").
    github: Vec<(&'static str, &'static str, &'static str, Option<String>)>,
    /// The consultant, run by the server.
    consultant: super::consultant::View,
    /// Requests from `flow` waiting for approval: (code, what, machine, asked).
    approvals: Vec<(String, String, String, String)>,
    passkeys_available: bool,
    public_url: String,
    /// This server's glibc: the downloaded program needs one at least as new.
    glibc: String,
    passkeys: Vec<PasskeyView>,
    push_key: String,
    push_devices: Vec<DeviceView>,
    toggles: Vec<ToggleView>,
    clients: Vec<ClientView>,
    agent_tokens: Vec<TokenView>,
    sessions: Vec<SessionView>,
    /// The Build servers card.
    build_servers: String,
}

/// A machine with `flow` (the owner, 2026-11-17, D-190: "rename this to clients … and also show
/// what projects run on these devices").
struct ClientView {
    token: TokenView,
    /// An agent of one of its projects is online.
    online: bool,
    /// The projects set up there: (name, slug, folder, agent "online" / "last seen 2 h ago").
    projects: Vec<(String, String, String, String)>,
}

struct PersonView {
    id: i64,
    name: String,
    email: String,
    icon: String,
    admin: bool,
    you: bool,
    /// "since 7 Nov" (the admin) or "joined 2 days ago".
    since: String,
    /// "3 passkeys".
    passkeys: String,
}

struct SectionLink {
    key: &'static str,
    label: &'static str,
    icon: &'static str,
    badge: Option<String>,
}

/// The sections: (group, [(key, label, icon)]) (the owner, 2026-11-17, from the mockups).
/// A section: (key, label, icon).
type Section = (&'static str, &'static str, &'static str);

const SECTIONS: &[(&str, &[Section])] = &[
    // Yours alone (D-270); the rest is the same for everyone on this AgentFlow (D-272).
    (
        "You",
        &[
            ("profile", "Profile", "person"),
            ("passkeys", "Passkeys", "key"),
            ("browsers", "Signed-in browsers", "window"),
            ("notifications", "Notifications", "bell"),
        ],
    ),
    ("People", &[("people", "People", "person")]),
    ("Machines", &[("clients", "Clients", "laptop"), ("builders", "Build servers", "server")]),
    ("Agents", &[("agents", "Agent tokens", "robot"), ("consultant", "Consultant", "message")]),
    ("Connections", &[("github", "GitHub", "git")]),
];

/// The section a key names, or None.
fn section_key(k: &str) -> Option<&'static str> {
    // Clients were called Devices until 2026-11-17; older links still find them.
    let k = if k == "devices" { "clients" } else { k };
    SECTIONS.iter().flat_map(|(_, l)| l.iter()).find(|(key, _, _)| *key == k).map(|(key, _, _)| *key)
}

/// Which section a notice belongs to (after a form post).
fn section_of_notice(done: &str) -> &'static str {
    match done {
        "passkey-removed" | "link-withdrawn-device" => "passkeys",
        "session-ended" => "browsers",
        "profile-saved" => "profile",
        "link-withdrawn" | "person-removed" => "people",
        "revoked" | "approved" | "denied" | "approve-failed" => "clients",
        "rotated" => "agents",
        "github-saved" | "github-removed" => "github",
        _ => "notifications",
    }
}

#[derive(Deserialize)]
struct PageQuery {
    /// The section to show.
    s: Option<String>,
    done: Option<String>,
    /// What happened on the Build servers card.
    bs: Option<String>,
    /// Signed in with a setup or device link where passkeys work later: add one now.
    add_passkey: Option<String>,
}

/// Messages after a form post; the query names one of these, never free text.
fn notice_for(key: &str) -> Option<&'static str> {
    Some(match key {
        "passkey-removed" => "Passkey removed.",
        "profile-saved" => "Saved. This is how you show up from now on.",
        "link-withdrawn" | "link-withdrawn-device" => "Withdrawn: the link doesn't work any more.",
        "person-removed" => "Removed: they're signed out everywhere and can't sign in again.",
        "device-removed" => "That device won't get notifications any more.",
        "test-sent" => "Test notification sent to your devices that have notifications on.",
        "test-none" => "No device has notifications on yet.",
        "notify-saved" => "Notification settings saved.",
        "revoked" => "Token revoked. Whatever used it can no longer reach AgentFlow.",
        "rotated" => "New token written to its file; the old one is revoked. The agent picks it up when it reconnects.",
        "session-ended" => "That browser is signed out.",
        "approved" => "Approved and done. The terminal that asked has it now.",
        "denied" => "Denied. Nothing was done.",
        "approve-failed" => "Approved, but it failed; the terminal that asked says why.",
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
    // The one asked for; after a form, the one it came from; else Notifications.
    let explicit = q.s.as_deref().and_then(section_key).or_else(|| {
        if q.add_passkey.is_some() {
            Some("passkeys")
        } else if q.bs.is_some() {
            Some("builders")
        } else {
            q.done.as_deref().filter(|d| notice_for(d).is_some()).map(section_of_notice)
        }
    });
    render_page(&state, &actor, notice, q.bs.as_deref(), explicit, None).await
}

/// A link just made in Settings, shown once (only its hash is kept).
struct Made {
    /// "device" (yours), "person" (someone else's device, by the admin) or "invite".
    what: &'static str,
    /// For "person": whose account.
    person: i64,
    url: String,
    until: String,
    /// The link's QR code, as SVG.
    qr: String,
    /// Its row, for Withdraw.
    id: i64,
}

/// Makes a one-time link and returns it with its row id.
async fn make_link(
    state: &AppState,
    kind: &'static str,
    person: Option<i64>,
    made_by: i64,
    minutes: i64,
) -> anyhow::Result<(String, i64, i64)> {
    let (secret, id) = state
        .db
        .call(move |c| {
            let s = auth::create_login_link(c, kind, person, Some(made_by), minutes)?;
            Ok((s, c.last_insert_rowid()))
        })
        .await?;
    let url = format!("{}/{kind}/{secret}", state.config.public_url.trim_end_matches('/'));
    Ok((url, id, now_ms() + minutes * 60_000))
}

fn made(what: &'static str, person: i64, (url, id, until): (String, i64, i64)) -> Made {
    let tz = crate::tz::Tz::default();
    let until = if until - now_ms() < 24 * 3_600_000 {
        tz.hm(until)
    } else {
        format!("{} {}", tz.day_label(until).split(' ').next().unwrap_or(""), tz.short(until))
    };
    Made { what, person, qr: super::qr::svg(&url), url, until, id }
}

/// Settings › Passkeys › Sign in on another device (D-274): a link that adds a passkey to your own
/// account, for 15 minutes, with its QR code.
async fn device_link(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let me = actor.person().unwrap_or(0);
    let link = make_link(&state, auth::purpose::DEVICE, Some(me), me, auth::LINK_MINUTES).await?;
    log(&state, &actor, &ip, Action::Write, "a link for another device made".into()).await;
    render_page(&state, &actor, None, None, Some("passkeys"), Some(made("device", me, link))).await
}

/// Settings › People › Invite someone: only the admin (D-273); a link for 7 days.
async fn invite(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
) -> AppResult<Response> {
    actor.require_admin()?;
    let me = actor.person().unwrap_or(0);
    let link = make_link(&state, auth::purpose::INVITE, None, me, auth::INVITE_MINUTES).await?;
    log(&state, &actor, &ip, Action::Write, "an invite made".into()).await;
    render_page(&state, &actor, None, None, Some("people"), Some(made("invite", 0, link))).await
}

/// Settings › People › Sign-in link: the admin makes a device link for someone who lost theirs.
async fn person_link(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    actor.require_admin()?;
    let me = actor.person().unwrap_or(0);
    let person = state.db.call(move |c| crate::people::get(c, id)).await?.ok_or(AppError::NotFound)?;
    let link = make_link(&state, auth::purpose::DEVICE, Some(person.id), me, auth::LINK_MINUTES).await?;
    log(&state, &actor, &ip, Action::Write, format!("a sign-in link made for {}", person.name())).await;
    render_page(&state, &actor, None, None, Some("people"), Some(made("person", person.id, link))).await
}

/// Settings › People › Remove (the security review: nobody could be removed). Only the admin.
async fn person_remove(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Path(id): Path<i64>,
) -> AppResult<Redirect> {
    actor.require_admin()?;
    let name = state
        .db
        .call(move |c| {
            let tx = c.transaction()?;
            let name = crate::people::get(&tx, id)?.map(|p| p.name());
            crate::people::remove(&tx, id)?;
            tx.commit()?;
            Ok(name)
        })
        .await
        .map_err(|e| AppError::BadRequest(format!("{e:#}")))?;
    log(&state, &actor, &ip, Action::Write, format!("removed {}", name.unwrap_or_default())).await;
    Ok(Redirect::to("/settings?done=person-removed"))
}

/// Withdraws a link that isn't used yet: one you made, one for you, or (the admin) an invite.
async fn link_withdraw(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Path(id): Path<i64>,
) -> AppResult<Redirect> {
    actor.require_owner()?;
    let me = actor.person().unwrap_or(0);
    let admin = matches!(actor, Actor::Owner { admin: true, .. });
    let link = state
        .db
        .call(move |c| {
            let link: Option<(String, Option<i64>)> = c
                .query_row(
                    "UPDATE login_links SET used_at = ?1 WHERE rowid = ?2 AND used_at IS NULL
                       AND (made_by = ?3 OR person_id = ?3 OR (?4 AND purpose = 'invite'))
                     RETURNING purpose, person_id",
                    params![now_ms(), id, me, admin],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok();
            Ok(link)
        })
        .await?;
    let Some((purpose, person)) = link else { return Err(AppError::NotFound) };
    log(&state, &actor, &ip, Action::Write, format!("a {purpose} link withdrawn")).await;
    // Back where it was made: your own device link under Passkeys, the rest under People.
    Ok(Redirect::to(if person == Some(me) {
        "/settings?done=link-withdrawn-device"
    } else {
        "/settings?done=link-withdrawn"
    }))
}

async fn render_page(
    state: &AppState,
    actor: &Actor,
    notice: Option<&'static str>,
    bs: Option<&str>,
    explicit: Option<&'static str>,
    made: Option<Made>,
) -> AppResult<Response> {
    let current = match actor {
        Actor::Owner { session_id, .. } => *session_id,
        _ => 0,
    };
    // Passkeys, browsers and devices are the person's own (D-270).
    let me = actor.person().unwrap_or(0);
    let now = now_ms();
    let (passkeys, devices, toggles, tokens, sessions, projects) = state
        .db
        .call(move |c| {
            let passkeys = passkey::list(c, me)?;
            let devices = push::devices(c, me)?;
            let toggles = Kind::ALL
                .iter()
                .map(|&k| Ok((k, push::is_enabled(c, Some(me), k)?)))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let tokens = auth::list_tokens(c)?;
            let mut stmt = c.prepare(
                "SELECT id, method, user_agent, last_seen_at FROM web_sessions
                 WHERE revoked_at IS NULL AND expires_at > ?1 AND person_id = ?2 ORDER BY last_seen_at DESC",
            )?;
            let sessions = stmt
                .query_map([now, me], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let projects = crate::projects::list(c)?;
            Ok((passkeys, devices, toggles, tokens, sessions, projects))
        })
        .await?;

    let when = |t: Option<i64>| t.map(|t| ago(t, now)).unwrap_or_else(|| "never".into());
    let active = tokens.into_iter().filter(|t| t.revoked_at.is_none());
    let machine_of = |slug: &Option<String>| {
        let slug = slug.as_deref()?;
        projects.iter().find(|p| p.slug == slug).map(|p| p.machine.clone())
    };
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
                "builder" => "builder: runs builds on its machine",
                _ => "read, CLI and viber-run",
            },
            rotate: t
                .kind
                .parse::<auth::TokenKind>()
                .is_ok_and(|k| tokens::rotates_here(k, machine_of(&t.project).as_deref())),
            used: when(t.last_used_at),
            added: ago(t.created_at, now),
            from: t.last_used_from.clone().unwrap_or_default(),
            hint: if t.hint.is_empty() { String::new() } else { format!("…{}", t.hint) },
            kind: t.kind,
        })
        .partition(|t| t.kind == "device");

    let approvals: Vec<(String, String, String, String)> = state
        .db
        .call(|c| crate::approvals::pending(c))
        .await?
        .iter()
        .map(|a| (a.code.clone(), crate::approvals::describe(a), a.machine.clone(), ago(a.created_at, now)))
        .collect();
    let section = explicit.unwrap_or("profile");
    // Who you are, and the others (D-270, D-272).
    let admin = matches!(actor, Actor::Owner { admin: true, .. });
    let (me_person, people_rows, invites) = state
        .db
        .call(move |c| {
            let everyone = crate::people::list(c)?;
            let mut rows = Vec::new();
            for p in everyone {
                let (keys, _) = crate::people::counts(c, p.id)?;
                rows.push((p, keys));
            }
            let mut st = c.prepare(
                "SELECT rowid, created_at, expires_at FROM login_links
                 WHERE purpose = 'invite' AND used_at IS NULL AND expires_at > ?1 ORDER BY created_at DESC",
            )?;
            let invites = st
                .query_map([now], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok((crate::people::get(c, me)?, rows, invites))
        })
        .await?;
    let tz = crate::tz::Tz::default();
    let people: Vec<PersonView> = people_rows
        .into_iter()
        .map(|(p, keys)| PersonView {
            id: p.id,
            name: p.name(),
            email: p.email.clone(),
            icon: p.icon.clone(),
            admin: p.admin,
            you: p.id == me,
            since: if p.invited_by.is_some() {
                format!("joined {}", ago(p.created_at, now))
            } else {
                format!("since {}", tz.day(p.created_at))
            },
            passkeys: format!("{keys} passkey{}", if keys == 1 { "" } else { "s" }),
        })
        .collect();
    let admin_name = people.iter().find(|p| p.admin).map(|p| p.name.clone()).unwrap_or_default();
    let invites: Vec<(i64, String, String)> =
        invites.into_iter().map(|(id, made, until)| (id, ago(made, now), tz.short(until))).collect();
    let groups = SECTIONS
        .iter()
        .map(|(g, links)| {
            let links = links
                .iter()
                .map(|&(key, label, icon)| SectionLink {
                    key,
                    label,
                    icon,
                    badge: if key == "clients" && !approvals.is_empty() {
                        Some(format!("{} waiting", approvals.len()))
                    } else if key == "people" && people.len() > 1 {
                        Some(people.len().to_string())
                    } else {
                        None
                    },
                })
                .collect();
            (*g, links)
        })
        .collect();
    let mut nav = nav(state, actor, "/settings").await;
    // On the phone a section is its own screen: back to the list.
    if explicit.is_some() {
        nav.back = Some(("/settings".into(), "Settings".into()));
    }
    let consultant_hint = agent_tokens
        .iter()
        .find(|t| t.kind == "admin" && t.holder == crate::consultant::NAME && !t.hint.is_empty())
        .map(|t| t.hint.clone());
    // A client's projects: those set up on its host (`flow project setup` records the machine).
    let clients = device_tokens
        .into_iter()
        .map(|t| {
            let host = t.holder.rsplit('@').next().unwrap_or(&t.holder).to_lowercase();
            let mine: Vec<_> = projects
                .iter()
                .filter(|p| p.status != "archived")
                .filter(|p| {
                    !host.is_empty() && (p.machine.to_lowercase() == host || p.agent.machine.to_lowercase() == host)
                })
                .collect();
            ClientView {
                online: mine.iter().any(|p| p.agent.online(now)),
                projects: mine
                    .iter()
                    .map(|p| {
                        let agent = if p.agent.online(now) {
                            "agent online".to_owned()
                        } else {
                            p.agent.seen_at.map(|t| format!("agent last seen {}", ago(t, now))).unwrap_or_default()
                        };
                        (p.name.clone(), p.slug.clone(), p.checkout.clone(), agent)
                    })
                    .collect(),
                token: t,
            }
        })
        .collect();
    let page = SettingsPage {
        nav,
        notice,
        profile: me_person.map(|p| (p.username.unwrap_or_default(), p.email, p.icon)).unwrap_or_default(),
        people,
        admin,
        admin_name,
        invites,
        made,
        groups,
        section,
        chosen: explicit.is_some(),
        github: {
            use crate::secrets::{GitHub, token};
            let login = |k: &'static str| async move {
                state
                    .db
                    .call(move |c| {
                        Ok(c.query_row("SELECT value FROM settings WHERE key = ?1", [k], |r| r.get::<_, String>(0))
                            .ok())
                    })
                    .await
                    .ok()
                    .flatten()
            };
            let show = |t: Option<String>, l: Option<String>| {
                t.map(|t| format!("…{}{}", crate::secrets::hint(&t), l.map(|l| format!(" · {l}")).unwrap_or_default()))
            };
            vec![
                (
                    "own",
                    "Your repositories",
                    "merging, tags, reading PRs",
                    show(token(GitHub::Own), login("github.login.own").await),
                ),
                (
                    "external",
                    "Other people's repositories",
                    "sending the contributions you approve",
                    show(token(GitHub::External), login("github.login.external").await),
                ),
            ]
        },
        consultant: super::consultant::View { token_hint: consultant_hint, ..super::consultant::view(state).await },
        approvals,
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
        clients,
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
    let Some(me) = actor.person() else { return json_error(StatusCode::FORBIDDEN, "sign in first") };
    match state
        .db
        .call(move |c| {
            let label = crate::people::get(c, me)?.map(|p| p.name()).unwrap_or_default();
            pk.start_registration(c, Some(me), &label, &format!("person-{me}"))
        })
        .await
    {
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
    let Some(me) = actor.person() else { return json_error(StatusCode::FORBIDDEN, "sign in first") };
    match state
        .db
        .call(move |c| {
            let (key, user) = pk.finish_registration(&body.id, &format!("person-{me}"), &body.credential)?;
            if user != crate::people::webauthn_id(c, me)? {
                anyhow::bail!("this enrolment was started for someone else");
            }
            crate::passkey::store(c, me, &body.name, &key)
        })
        .await
    {
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
    let me = actor.person().unwrap_or(0);
    if !state.db.call(move |c| passkey::remove(c, id, me)).await? {
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
    let Some(me) = actor.person() else { return json_error(StatusCode::FORBIDDEN, "sign in first") };
    match state.db.call(move |c| push::save_subscription(c, &body.subscription, &device, me)).await {
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
    let me = actor.person().unwrap_or(0);
    if !state.db.call(move |c| push::remove_device(c, id, me)).await? {
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
    let delivered = state.push.send_test(actor.person().unwrap_or(0), &n).await?;
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
    let login = github_login(&token)
        .await
        .map_err(|e| AppError::BadRequest(format!("GitHub didn't take that token: {e:#}")))?;
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
    Ok(Redirect::to("/settings?done=github-saved").into_response())
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
    Ok(Redirect::to("/settings?done=github-removed").into_response())
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
        anyhow::bail!(
            "{}",
            String::from_utf8_lossy(&out.stderr).replace(token, "…").trim().lines().last().unwrap_or("refused")
        );
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
    let me = actor.person().unwrap_or(0);
    state
        .db
        .call(move |c| {
            for k in Kind::ALL {
                push::set_enabled(c, me, k, on.contains(&k))?;
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
    // Refused for tokens that live on other machines: say why instead of a server error.
    let (new_id, path) = state
        .db
        .call(move |c| tokens::rotate(c, id, &dir))
        .await
        .map_err(|e| AppError::BadRequest(format!("{e:#}")))?;
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
    let me = actor.person().unwrap_or(0);
    // Only the person's own browsers.
    let n = state
        .db
        .call(move |c| {
            Ok(c.execute(
                "UPDATE web_sessions SET revoked_at = ?1 WHERE id = ?2 AND person_id = ?3 AND revoked_at IS NULL",
                params![now_ms(), id, me],
            )?)
        })
        .await?;
    if n == 0 {
        return Err(AppError::NotFound);
    }
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
