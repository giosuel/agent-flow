//! Add project (the owner, 2026-11-13): the commands to set a project up on the machine where its
//! agent runs (`flow project setup` there does it all), and for when there's no computer at hand,
//! a form that creates it here and hands out a setup code for later.

use askama::Template;
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use axum::{Extension, Form, Router};
use serde::Deserialize;

use super::pages::{Nav, nav, render};
use super::{AppResult, AppState, ClientIp};
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
    public_url: String,
    error: Option<String>,
    name: String,
    description: String,
    stack: String,
    platform: String,
    model: String,
    effort: String,
}

fn page(state: &AppState, nav: Nav, f: &NewForm, error: Option<String>) -> AppResult<Response> {
    render(NewProjectPage {
        nav: nav.back_to("/projects", "Projects"),
        public_url: state.config.public_url.trim_end_matches('/').to_owned(),
        error,
        name: f.name.clone(),
        description: f.description.clone(),
        stack: f.stack.clone(),
        platform: f.platform.clone(),
        model: if f.model.is_empty() { "Claude Opus 5.5".into() } else { f.model.clone() },
        effort: if f.effort.is_empty() { "high".into() } else { f.effort.clone() },
    })
}

async fn form(State(state): State<AppState>, Extension(actor): Extension<Actor>) -> AppResult<Response> {
    actor.require_owner()?;
    page(&state, nav(&state, &actor, "/projects").await, &NewForm::default(), None)
}

/// Creates the project here, without a folder or machine yet, and shows its setup code.
async fn create(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Form(f): Form<NewForm>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let name = f.name.trim().to_owned();
    if name.is_empty() || name.chars().count() > 60 {
        return page(&state, nav(&state, &actor, "/projects").await, &f, Some("Give the project a name (up to 60 characters).".into()));
    }
    let slug = if f.slug.trim().is_empty() { onboard::slug_for(&name) } else { f.slug.trim().to_owned() };
    let p = NewProject {
        name: name.clone(),
        slug: slug.clone(),
        folder: String::new(),
        description: f.description.trim().into(),
        stack: f.stack.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect(),
        platform: f.platform.trim().into(),
        model: f.model.trim().into(),
        effort: f.effort.trim().into(),
    };
    let created = state
        .db
        .call(move |c| {
            crate::projects::check_slug(&p.slug)?;
            if crate::projects::get(c, &p.slug)?.is_some() {
                anyhow::bail!("a project {} exists already; pick another name", p.slug);
            }
            let (id, _) = onboard::create_records(c, &p, None, None, false)?;
            onboard::new_setup_code(c, id)
        })
        .await;
    let code = match created {
        Ok(code) => code,
        Err(e) => return page(&state, nav(&state, &actor, "/projects").await, &f, Some(format!("{e:#}"))),
    };
    let e = Entry::new(&actor, Via::Page, Action::Write, format!("project {slug} created on the page, to be set up on a machine"))
        .project(slug.clone())
        .ip(Some(ip.0));
    activity::write(&state.db, e).await;
    render(super::projects::SetupCodePage {
        nav: nav(&state, &actor, "/projects").await,
        slug,
        name,
        code,
        public_url: state.config.public_url.trim_end_matches('/').to_owned(),
        minutes: onboard::SETUP_CODE_MINUTES,
    })
}
