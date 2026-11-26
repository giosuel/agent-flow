//! The one-time links people get in with (the owner, 2026-11-25: D-273, D-274):
//! - /setup/… (`sudo flow server setup`) makes the admin, or makes the admin anew when they lost
//!   their passkeys: then the admin's old passkeys go and their browsers are signed out;
//! - /invite/… (the admin, from Settings › People or `sudo flow server invite`) makes someone new;
//! - /device/… (Settings › Passkeys or `sudo flow server add-device`) adds a passkey for a new
//!   device to the account it names.
//!
//! Opening a link only shows its page: link previews (chat apps, mail scanners) fetch with GET and
//! must not use it up. Creating the passkey uses it and signs that browser in. Where passkeys can't
//! work (plain http on an IP address), the page signs in with the link itself instead.

use askama::Template;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use axum_extra::extract::CookieJar;
use rusqlite::Connection;
use webauthn_rs::prelude::{Passkey, RegisterPublicKeyCredential, Uuid};

use super::pages::VERSION;
use super::pages::{session_cookie, user_agent};
use super::{AppError, AppResult, AppState, ClientIp, json_error};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::{self, Actor, SessionLimits, purpose};
use crate::people;

pub fn routes() -> Router<AppState> {
    let mut r = Router::new();
    for kind in [purpose::SETUP, purpose::INVITE, purpose::DEVICE] {
        r = r
            .route(&format!("/{kind}/{{secret}}"), get(page).post(sign_in))
            .route(&format!("/{kind}/{{secret}}/passkey/start"), post(passkey_start))
            .route(&format!("/{kind}/{{secret}}/passkey/finish"), post(passkey_finish));
    }
    r
}

/// The link pages are public: whoever has the link may use it.
pub fn is_public(path: &str) -> bool {
    [purpose::SETUP, purpose::INVITE, purpose::DEVICE]
        .iter()
        .any(|k| path.strip_prefix('/').and_then(|p| p.strip_prefix(k)).is_some_and(|rest| rest.starts_with('/')))
}

/// Which kind of link a request is for, from its path: "setup", "invite" or "device".
fn kind_of(uri: &Uri) -> &'static str {
    let first = uri.path().trim_start_matches('/').split('/').next().unwrap_or("");
    [purpose::SETUP, purpose::INVITE, purpose::DEVICE].into_iter().find(|k| *k == first).unwrap_or("")
}

#[derive(Template)]
#[template(path = "link.html")]
struct LinkPage {
    version: &'static str,
    kind: &'static str,
    secret: String,
    /// The link works: not used, not expired, of this kind.
    ok: bool,
    /// Passkeys work at this address.
    passkeys: bool,
    /// For setup: the admin who is made anew; for a device link: whose account it adds to.
    person: String,
    /// For an invite: who invited.
    inviter: String,
    /// "15:55" or "Wed 2 Dec".
    until: String,
}

/// Whose account a link enrols a passkey for: the person it names (a device link, the admin set
/// up again), or None for someone new (the first setup, an invite).
fn registrant(link: &auth::Link) -> Option<i64> {
    if link.purpose == purpose::INVITE { None } else { link.person }
}

async fn page(State(state): State<AppState>, uri: Uri, Path(secret): Path<String>) -> AppResult<Response> {
    let kind = kind_of(&uri);
    let s = secret.clone();
    let found = state
        .db
        .call(move |c| {
            let Some(link) = auth::peek_login_link(c, &s)? else { return Ok(None) };
            if link.purpose != kind {
                return Ok(None);
            }
            let name = |id: Option<i64>| -> anyhow::Result<String> {
                Ok(id.map(|i| people::get(c, i)).transpose()?.flatten().map(|p| p.name()).unwrap_or_default())
            };
            let person = name(registrant(&link))?;
            let inviter = name(link.made_by)?;
            Ok(Some((link.expires_at, person, inviter)))
        })
        .await?;
    let tz = crate::tz::Tz::default();
    let mut page = LinkPage {
        version: VERSION,
        kind,
        secret,
        ok: found.is_some(),
        passkeys: state.passkeys.available(),
        person: String::new(),
        inviter: String::new(),
        until: String::new(),
    };
    let Some((expires, person, inviter)) = found else {
        return Ok((StatusCode::GONE, Html(page.render()?)).into_response());
    };
    let soon = expires - crate::util::now_ms() < 24 * 3_600_000;
    page.until = if soon {
        tz.hm(expires)
    } else {
        format!("{} {}", tz.day_label(expires).split(' ').next().unwrap_or(""), tz.day(expires))
    };
    page.person = person;
    page.inviter = inviter;
    Ok(Html(page.render()?).into_response())
}

/// Uses a link up and signs the browser in as its person, made now when the link is for someone
/// new. With a passkey, keeps it for that person; setup with an admin removes the admin's old
/// passkeys and signs out their browsers first. One transaction: a refused link stays unused.
fn redeem(
    c: &mut Connection,
    secret: &str,
    kind: &str,
    key: Option<(&Passkey, Uuid, &str)>,
    limits: &SessionLimits,
    ua: Option<&str>,
    ip: &str,
) -> anyhow::Result<Option<(i64, String, bool)>> {
    let tx = c.transaction()?;
    let Some(link) = auth::consume_login_link(&tx, secret)? else { return Ok(None) };
    if link.purpose != kind {
        return Ok(None);
    }
    let handle = key.map(|(_, u, _)| u).unwrap_or_else(Uuid::new_v4);
    let person = match kind {
        purpose::SETUP => {
            // Only for the state it was made for (the security review): the first setup while
            // there's no admin, or this admin anew. Any other setup link stops working.
            let id = match (people::admin(&tx)?, link.person) {
                (Some(a), Some(p)) if a.id == p => {
                    people::reset_admin(&tx, a.id)?;
                    a.id
                }
                (None, None) => people::create(&tx, true, None, handle)?,
                _ => anyhow::bail!(
                    "this setup link was made before the admin changed; run `sudo flow server setup` again for a new one"
                ),
            };
            tx.execute(
                "UPDATE login_links SET used_at = ?1 WHERE purpose = 'setup' AND used_at IS NULL",
                [crate::util::now_ms()],
            )?;
            id
        }
        purpose::INVITE => {
            // Only the admin invites (D-273), and only while they're still the admin.
            let maker = link.made_by.map(|m| people::get(&tx, m)).transpose()?.flatten();
            if !maker.is_some_and(|m| m.admin) {
                anyhow::bail!("this invite's admin isn't the admin any more");
            }
            people::create(&tx, false, link.made_by, handle)?
        }
        _ => {
            let id = link.person.map(|p| people::get(&tx, p)).transpose()?.flatten();
            id.map(|p| p.id).ok_or_else(|| anyhow::anyhow!("this link's person was removed"))?
        }
    };
    if let Some((passkey, user, name)) = key {
        if user != people::webauthn_id(&tx, person)? {
            anyhow::bail!("this passkey was made for someone else");
        }
        crate::passkey::store(&tx, person, name, passkey)?;
    }
    let method = if key.is_some() { "passkey" } else { "link" };
    let session = auth::create_session(&tx, person, method, limits, ua, Some(ip))?;
    let profile = people::get(&tx, person)?.is_some_and(|p| p.has_profile());
    tx.commit()?;
    Ok(Some((person, session, profile)))
}

/// Where a browser goes once it used a link: someone new picks who they are first (the first
/// sign-in's step 2), then adds a passkey if the link signed them in without one.
fn next(with_passkey: bool, profile: bool) -> &'static str {
    match (with_passkey, profile) {
        (true, true) => "/",
        (true, false) => "/welcome",
        (false, true) => "/settings?add_passkey=1#passkeys",
        (false, false) => "/welcome?next=/settings?add_passkey=1",
    }
}

async fn log_use(state: &AppState, kind: &str, person: Option<i64>, ip: &str, ok: bool, why: String) {
    let what = match (ok, person) {
        (true, Some(p)) => {
            let name = state.db.call(move |c| people::get(c, p)).await.ok().flatten().map(|p| p.name());
            format!("{kind} link used: {} signed in{why}", name.unwrap_or_else(|| format!("person {p}")))
        }
        _ => format!("{kind} link refused{why}"),
    };
    let e = Entry::new(&Actor::Anonymous, Via::Page, if ok { Action::Login } else { Action::Denied }, what)
        .ip(Some(ip.to_owned()));
    activity::write(&state.db, e).await;
}

fn gone(kind: &str) -> Response {
    let hint = match kind {
        purpose::INVITE => "Ask the admin for a new invite.",
        purpose::DEVICE => {
            "Make a new one in Settings › Passkeys on a signed-in device, or with `sudo flow server add-device`."
        }
        _ => "Run `sudo flow server setup` on the server for a new one.",
    };
    (StatusCode::GONE, format!("This link has expired or was already used. {hint}")).into_response()
}

/// Signs in with the link itself, where passkeys can't be made; the passkey comes later.
async fn sign_in(
    State(state): State<AppState>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    uri: Uri,
    headers: HeaderMap,
    jar: CookieJar,
    Path(secret): Path<String>,
) -> AppResult<Response> {
    let kind = kind_of(&uri);
    // Where passkeys work a link ends in one (the security review): signing in with the link
    // alone is only for addresses where browsers refuse passkeys.
    if state.passkeys.available() {
        return Err(AppError::BadRequest("Create a passkey on this page: this link signs in with one.".into()));
    }
    let (limits, ua, ip2) = (state.limits(), user_agent(&headers), ip.clone());
    let got = state.db.call(move |c| redeem(c, &secret, kind, None, &limits, ua.as_deref(), &ip2)).await;
    match got {
        Ok(Some((person, session, profile))) => {
            log_use(&state, kind, Some(person), &ip, true, String::new()).await;
            let jar = jar.add(session_cookie(&state, session));
            Ok((jar, Redirect::to(next(false, profile))).into_response())
        }
        Ok(None) => {
            log_use(&state, kind, None, &ip, false, ": unknown, used or expired".into()).await;
            Ok(gone(kind))
        }
        Err(e) => {
            log_use(&state, kind, None, &ip, false, format!(": {e:#}")).await;
            Err(AppError::BadRequest(format!("{e:#}")))
        }
    }
}

/// The link's ceremonies are told apart from Settings' and from other links' by the link.
fn client(secret: &str) -> String {
    format!("link-{}", &crate::util::hex(&crate::util::hash_secret(secret))[..16])
}

async fn passkey_start(State(state): State<AppState>, uri: Uri, Path(secret): Path<String>) -> Response {
    let kind = kind_of(&uri);
    let pk = state.passkeys.clone();
    let started = state
        .db
        .call(move |c| {
            let Some(link) = auth::peek_login_link(c, &secret)? else { return Ok(None) };
            if link.purpose != kind {
                return Ok(None);
            }
            let who = registrant(&link);
            let label = who.map(|p| people::get(c, p)).transpose()?.flatten().map(|p| p.name()).unwrap_or_default();
            Ok(Some(pk.start_registration(c, who, &label, &client(&secret))?))
        })
        .await;
    match started {
        Ok(Some((id, options))) => Json(serde_json::json!({"id": id, "options": options})).into_response(),
        Ok(None) => json_error(StatusCode::GONE, "this link has expired or was already used"),
        Err(e) => json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

#[derive(serde::Deserialize)]
struct Finish {
    id: String,
    #[serde(default)]
    name: String,
    credential: RegisterPublicKeyCredential,
}

async fn passkey_finish(
    State(state): State<AppState>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    uri: Uri,
    headers: HeaderMap,
    jar: CookieJar,
    Path(secret): Path<String>,
    Json(body): Json<Finish>,
) -> Response {
    let kind = kind_of(&uri);
    let pk = state.passkeys.clone();
    let (limits, ua, ip2) = (state.limits(), user_agent(&headers), ip.clone());
    let got = state
        .db
        .call(move |c| {
            let (key, user) = pk.finish_registration(&body.id, &client(&secret), &body.credential)?;
            let name = if body.name.trim().is_empty() { "Passkey" } else { body.name.trim() };
            redeem(c, &secret, kind, Some((&key, user, name)), &limits, ua.as_deref(), &ip2)
        })
        .await;
    match got {
        Ok(Some((person, session, profile))) => {
            log_use(&state, kind, Some(person), &ip, true, " with a new passkey".into()).await;
            let jar = jar.add(session_cookie(&state, session));
            (jar, Json(serde_json::json!({"ok": true, "next": next(true, profile)}))).into_response()
        }
        Ok(None) => {
            log_use(&state, kind, None, &ip, false, ": unknown, used or expired".into()).await;
            json_error(StatusCode::GONE, "this link has expired or was already used")
        }
        Err(e) => {
            log_use(&state, kind, None, &ip, false, format!(": {e:#}")).await;
            json_error(StatusCode::BAD_REQUEST, format!("{e:#}"))
        }
    }
}
