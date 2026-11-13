//! The consultant in Settings: on and off, its state, how to reach it (see crate::consultant). A
//! watchdog keeps it running while it's on, and reads its conversations for the Log, Usage and
//! Conversations.

use axum::extract::State;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::post;
use axum::{Extension, Router};

use super::{AppResult, AppState, ClientIp};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::consultant;

pub fn routes() -> Router<AppState> {
    Router::new().route("/settings/consultant/on", post(on)).route("/settings/consultant/off", post(off))
}

/// What the Settings section shows.
pub struct View {
    pub on: bool,
    pub running: bool,
    pub link: Option<String>,
    pub attach: String,
    pub error: Option<String>,
}

pub async fn view(state: &AppState) -> View {
    let folder = consultant::dir(&state.config.data_dir());
    let f2 = folder.clone();
    let (on, link, error) = state
        .db
        .call(move |c| {
            let error: Option<String> =
                c.query_row("SELECT value FROM settings WHERE key = 'consultant.error'", [], |r| r.get(0)).ok();
            Ok((consultant::is_on(c)?, consultant::session_link(c, &f2)?, error.filter(|e| !e.is_empty())))
        })
        .await
        .unwrap_or((false, None, None));
    View {
        on,
        running: consultant::running(&state.config.data_dir()).await,
        link,
        attach: consultant::attach_command(&state.config.data_dir()),
        error,
    }
}

async fn note_error(state: &AppState, e: &str) {
    let e = e.to_owned();
    let _ = state
        .db
        .call(move |c| {
            c.execute("INSERT INTO settings (key, value) VALUES ('consultant.error', ?1) ON CONFLICT (key) DO UPDATE SET value = excluded.value", [e])?;
            Ok(())
        })
        .await;
}

/// Sets its folder up with a fresh token and starts it.
async fn bring_up(state: &AppState) -> anyhow::Result<()> {
    consultant::login_ok().await?;
    let data = state.config.data_dir();
    let token = state.db.call(consultant::new_token).await?;
    let folder = consultant::write_folder(&data, &token, &state.config.public_url, &state.config.flow_bin())?;
    let resume = state.db.call(|c| consultant::resume_first(c)).await?;
    let history = consultant::has_history(&crate::usage::root(&state.config), &folder);
    consultant::start(&data, &folder, resume.as_deref(), history).await?;
    if resume.is_some() {
        state
            .db
            .call(|c| {
                c.execute("DELETE FROM settings WHERE key = 'consultant.resume'", [])?;
                Ok(())
            })
            .await?;
    }
    note_error(state, "").await;
    Ok(())
}

async fn on(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
) -> AppResult<Response> {
    actor.require_owner()?;
    if let Err(e) = consultant::login_ok().await {
        note_error(&state, &format!("{e:#}")).await;
        return Ok(Redirect::to("/settings#s-consultant").into_response());
    }
    state.db.call(|c| consultant::set_on(c, true)).await?;
    if !consultant::running(&state.config.data_dir()).await
        && let Err(e) = bring_up(&state).await
    {
        note_error(&state, &format!("{e:#}")).await;
    }
    activity::write(&state.db, Entry::new(&actor, Via::Page, Action::Write, "consultant switched on").ip(Some(ip.0)))
        .await;
    Ok(Redirect::to("/settings#s-consultant").into_response())
}

async fn off(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
) -> AppResult<Response> {
    actor.require_owner()?;
    state.db.call(|c| consultant::set_on(c, false)).await?;
    let _ = consultant::stop(&state.config.data_dir()).await;
    activity::write(&state.db, Entry::new(&actor, Via::Page, Action::Write, "consultant switched off").ip(Some(ip.0)))
        .await;
    Ok(Redirect::to("/settings#s-consultant").into_response())
}

/// Every minute while it's on: restart it if it's gone (it continues its conversation), and read
/// what's new in its conversations.
pub fn spawn_watchdog(state: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tick.tick().await;
            let on = state.db.call(|c| consultant::is_on(c)).await.unwrap_or(false);
            if !on {
                continue;
            }
            if !consultant::running(&state.config.data_dir()).await {
                tracing::info!("the consultant isn't running; starting it");
                if let Err(e) = bring_up(&state).await {
                    tracing::warn!("the consultant couldn't start: {e:#}");
                    note_error(&state, &format!("{e:#}")).await;
                }
            }
            let (root, folder) = (crate::usage::root(&state.config), consultant::dir(&state.config.data_dir()));
            if let Err(e) = state.db.call(move |c| consultant::ingest(c, &root, &folder)).await {
                tracing::warn!("the consultant's conversations: {e:#}");
            }
        }
    });
}
