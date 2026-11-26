//! Who you are on this AgentFlow (the owner, 2026-11-25, D-270): at the first sign-in a person
//! picks a username, an email and one of the 15 icons (/welcome), and changes them later in
//! Settings › Profile.

use askama::Template;
use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Form, Router};
use serde::Deserialize;

use super::pages::VERSION;
use super::{AppResult, AppState, ClientIp};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::people;

pub fn routes() -> Router<AppState> {
    Router::new().route("/welcome", get(welcome).post(welcome_save)).route("/settings/profile", post(settings_save))
}

/// Pages someone without a profile may still open: the welcome page itself, signing out, and what
/// pages load.
pub fn open_before_profile(path: &str) -> bool {
    path == "/welcome" || path == "/logout" || path.starts_with("/static/") || path == "/favicon.svg"
}

#[derive(Template)]
#[template(path = "welcome.html")]
struct Welcome {
    version: &'static str,
    username: String,
    email: String,
    icon: String,
    next: String,
    error: Option<String>,
}

#[derive(Deserialize)]
struct NextQuery {
    next: Option<String>,
}

/// Only a path on this site: `/something`, never `//elsewhere` (the same check as every `next`:
/// printable ASCII only, so a tab can't turn `/\t/x` into `//x`).
fn safe_next(next: Option<&str>) -> String {
    super::local_path(next).unwrap_or_else(|| "/".to_owned())
}

async fn welcome(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<NextQuery>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let me = actor.person().unwrap_or(0);
    let person = state.db.call(move |c| people::get(c, me)).await?;
    let p = person.unwrap_or_else(|| people::Person {
        id: me,
        username: None,
        email: String::new(),
        icon: String::new(),
        admin: false,
        invited_by: None,
        created_at: 0,
    });
    let page = Welcome {
        version: VERSION,
        username: p.username.unwrap_or_default(),
        email: p.email,
        icon: p.icon,
        next: safe_next(q.next.as_deref()),
        error: None,
    };
    Ok(Html(page.render()?).into_response())
}

#[derive(Deserialize)]
struct ProfileForm {
    username: String,
    email: String,
    #[serde(default)]
    icon: String,
    #[serde(default)]
    next: Option<String>,
}

async fn save(state: &AppState, actor: &Actor, ip: &str, form: &ProfileForm) -> anyhow::Result<()> {
    let me = actor.person().unwrap_or(0);
    let (u, e, i) = (form.username.clone(), form.email.clone(), form.icon.clone());
    state.db.call(move |c| people::set_profile(c, me, &u, &e, &i)).await?;
    let entry =
        Entry::new(actor, Via::Page, Action::Write, format!("profile set: {}", form.username.trim().to_lowercase()))
            .ip(Some(ip.to_owned()));
    activity::write(&state.db, entry).await;
    Ok(())
}

async fn welcome_save(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Form(form): Form<ProfileForm>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let next = safe_next(form.next.as_deref());
    match save(&state, &actor, &ip, &form).await {
        Ok(()) => Ok(Redirect::to(&next).into_response()),
        Err(e) => {
            let page = Welcome {
                version: VERSION,
                username: form.username,
                email: form.email,
                icon: form.icon,
                next,
                error: Some(format!("{e:#}")),
            };
            Ok((axum::http::StatusCode::UNPROCESSABLE_ENTITY, Html(page.render()?)).into_response())
        }
    }
}

async fn settings_save(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Form(form): Form<ProfileForm>,
) -> AppResult<Response> {
    actor.require_owner()?;
    match save(&state, &actor, &ip, &form).await {
        Ok(()) => Ok(Redirect::to("/settings?s=profile&done=profile-saved").into_response()),
        Err(e) => Err(super::AppError::BadRequest(format!("{e:#}"))),
    }
}
