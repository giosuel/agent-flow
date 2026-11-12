//! A step's page (it opens in the record dialog): its state, the agent's summary once it's
//! concluded, and its tasks in order (the owner, 2026-11-12).

use askama::Template;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::get;
use axum::{Extension, Router};

use super::pages::{Nav, nav, render};
use super::{AppError, AppResult, AppState};
use crate::auth::Actor;
use crate::markdown;
use crate::projects;
use crate::records::Record;
use crate::tz::Tz;

pub fn routes() -> Router<AppState> {
    Router::new().route("/p/{slug}/steps/{id}", get(step))
}

pub struct Row {
    pub n: usize,
    pub short: String,
    pub url: String,
    pub title: String,
    pub state: String,
    pub est: String,
}

#[derive(Template)]
#[template(path = "step.html")]
struct StepPage {
    nav: Nav,
    project_name: String,
    project_slug: String,
    name: String,
    milestone: String,
    title: String,
    state: &'static str,
    when: String,
    summary_html: String,
    description: String,
    rows: Vec<Row>,
}

async fn step(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((slug, id)): Path<(String, i64)>,
    tz: Tz,
) -> AppResult<Response> {
    let page = state
        .db
        .call(move |c| {
            let Some(p) = projects::get(c, &slug)? else { return Ok(None) };
            let Ok(m) = projects::get_milestone(c, id) else { return Ok(None) };
            if m.project_id != p.id {
                return Ok(None);
            }
            let parent = match m.parent_id {
                Some(pid) => projects::get_milestone(c, pid)?.name,
                None => String::new(),
            };
            let tasks: Vec<Record> = crate::steps::tasks_of(c, m.id)?;
            Ok(Some((p, m, parent, tasks)))
        })
        .await?
        .ok_or(AppError::NotFound)?;
    let (p, m, parent, tasks) = page;
    let state_word = match m.state.as_str() {
        "closed" => "concluded",
        "active" => "active",
        _ => "planned",
    };
    let when = match (m.started_at, m.closed_at) {
        (Some(a), Some(b)) => format!("{} – {}", tz.short(a), tz.short(b)),
        (Some(a), None) => format!("started {}", tz.short(a)),
        (None, Some(b)) => format!("concluded {}", tz.short(b)),
        (None, None) => "not started yet".into(),
    };
    let rows = tasks
        .iter()
        .enumerate()
        .map(|(i, t)| Row {
            n: i + 1,
            short: t.short_id(),
            url: t.url(),
            title: t.title.clone(),
            state: t.state.clone(),
            est: t.str("estimate").to_owned(),
        })
        .collect();
    render(StepPage {
        nav: nav(&state, &actor, "/projects").await.back_to(&format!("/p/{}?tab=tasks", p.slug), &p.name),
        project_name: p.name.clone(),
        project_slug: p.slug.clone(),
        name: m.name.clone(),
        milestone: parent,
        title: m.title.clone(),
        state: state_word,
        when,
        summary_html: markdown::to_html(&m.summary),
        description: m.description.clone(),
        rows,
    })
}
