//! The New project button: a form, then what was done and the commands left for the owner.

use askama::Template;
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::{Extension, Form, Router};
use serde::Deserialize;

use super::pages::{Nav, nav, render};
use super::{AppError, AppResult, AppState, ClientIp};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::onboard::{self, NewProject};

pub fn routes() -> Router<AppState> {
    Router::new().route("/projects/new", get(form).post(create))
}

#[derive(Deserialize, Default)]
struct NewForm {
    name: String,
    #[serde(default)]
    slug: String,
    #[serde(default)]
    folder: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    stack: String,
    #[serde(default)]
    platform: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    effort: String,
}

#[derive(Template)]
#[template(path = "new_project.html")]
struct NewProjectPage {
    nav: Nav,
    host: String,
    error: Option<String>,
    name: String,
    folder: String,
    description: String,
    stack: String,
    platform: String,
    model: String,
    effort: String,
    done: Option<onboard::Outcome>,
}

fn page(nav: Nav, f: &NewForm, error: Option<String>, done: Option<onboard::Outcome>) -> AppResult<Response> {
    render(NewProjectPage {
        nav,
        host: crate::machine::hostname(),
        error,
        name: f.name.clone(),
        folder: f.folder.clone(),
        description: f.description.clone(),
        stack: f.stack.clone(),
        platform: f.platform.clone(),
        model: if f.model.is_empty() { "Claude Opus 5.5".into() } else { f.model.clone() },
        effort: if f.effort.is_empty() { "high".into() } else { f.effort.clone() },
        done,
    })
}

async fn form(State(state): State<AppState>, Extension(actor): Extension<Actor>) -> AppResult<Response> {
    actor.require_owner()?;
    page(nav(&state, &actor, "/projects").await, &NewForm::default(), None, None)
}

async fn create(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Form(f): Form<NewForm>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let mut p = NewProject {
        name: f.name.trim().into(),
        slug: f.slug.trim().into(),
        folder: f.folder.trim().into(),
        description: f.description.trim().into(),
        stack: f.stack.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect(),
        platform: f.platform.trim().into(),
        model: f.model.trim().into(),
        effort: f.effort.trim().into(),
    };
    let folder = match onboard::check(&mut p) {
        Ok(folder) => folder,
        Err(e) => return page(nav(&state, &actor, "/projects").await, &f, Some(format!("{e:#}")), None),
    };
    let (p2, f2) = (p.clone(), folder.clone());
    let secret = match state.db.call(move |c| onboard::create_records(c, &p2, &f2)).await {
        Ok((_, secret)) => secret,
        Err(e) => return page(nav(&state, &actor, "/projects").await, &f, Some(format!("{e:#}")), None),
    };
    let (token_dir, claude, url) =
        (state.config.token_dir(), crate::usage::root(&state.config), state.config.public_url.clone());
    let p3 = p.clone();
    let files =
        tokio::task::spawn_blocking(move || onboard::create_files(&p3, &folder, &secret, &token_dir, &claude, &url))
            .await
            .map_err(|e| AppError::Internal(e.into()))?;
    let summary = match &files {
        Ok(_) => format!("project {} created from the New project button", p.slug),
        Err(e) => format!("project {} created, but setting up its files failed: {e:#}", p.slug),
    };
    let e = Entry::new(&actor, Via::Page, Action::Write, summary).project(p.slug.clone()).ip(Some(ip.0));
    activity::write(&state.db, e).await;
    match files {
        Ok(out) => page(nav(&state, &actor, "/projects").await, &f, None, Some(out)),
        Err(e) => page(
            nav(&state, &actor, "/projects").await,
            &f,
            Some(format!("The project {} exists now, but its files weren't set up: {e:#}", p.slug)),
            None,
        ),
    }
}
