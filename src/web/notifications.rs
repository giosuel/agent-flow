//! The bell (see `crate::notifications`): a panel under it (a sheet on the phone). Opening it marks
//! everything read; what was new keeps its dot while it's open.

use askama::Template;
use axum::extract::State;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};

use super::pages::render;
use super::{AppResult, AppState};
use crate::auth::Actor;
use crate::util::{ago, now_ms};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/notifications", get(page))
        .route("/notifications/panel", get(panel))
        .route("/notifications/read", post(read))
}

pub struct Row {
    pub title: String,
    pub body: String,
    pub url: String,
    pub icon: &'static str,
    pub tone: &'static str,
    pub meta: String,
    pub unread: bool,
}

fn rows(items: Vec<crate::notifications::Item>) -> Vec<Row> {
    let now = now_ms();
    items
        .into_iter()
        .map(|i| {
            let (icon, tone, what) = match i.kind.as_str() {
                "question" => ("question", "accent", "Question"),
                "build" => ("build", "ok", "Build"),
                "review" => ("merge", "ok", "To review"),
                "urgent" => ("bell", "bad", "Urgent"),
                _ => ("bell", "", "Test"),
            };
            Row {
                meta: format!("{what} · {}", ago(i.at, now)),
                title: i.title,
                body: i.body,
                url: i.url,
                icon,
                tone,
                unread: i.unread,
            }
        })
        .collect()
}

#[derive(Template)]
#[template(path = "notify_panel.html")]
struct Panel {
    fresh: Vec<Row>,
    earlier: Vec<Row>,
}

async fn load(state: &AppState) -> anyhow::Result<(Vec<Row>, Vec<Row>)> {
    let items = state.db.call(|c| crate::notifications::list(c, 100)).await?;
    Ok(rows(items).into_iter().partition(|r| r.unread))
}

/// The panel's content (the script fetches it, then marks everything read).
async fn panel(State(state): State<AppState>, Extension(actor): Extension<Actor>) -> AppResult<Response> {
    actor.require_owner()?;
    let (fresh, earlier) = load(&state).await?;
    render(Panel { fresh, earlier })
}

/// Notifications are the bell's panel only (the owner, 2026-11-18, D-204): a browser going to
/// this address gets the dashboard with the panel open (see `dialogs_over_pages`); anything else
/// is sent there.
async fn page() -> Response {
    Redirect::to("/").into_response()
}

async fn read(State(state): State<AppState>, Extension(actor): Extension<Actor>) -> AppResult<Response> {
    actor.require_owner()?;
    let n = state.db.call(|c| crate::notifications::mark_all_read(c)).await?;
    Ok(Json(serde_json::json!({"read": n})).into_response())
}
