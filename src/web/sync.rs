//! `flow sync` (the owner, 2026-11-13: decouple AgentFlow from the machine it runs on): a
//! project's hooks send what's new in its Claude Code conversations after each reply, subagent and
//! session end. flow reads the transcripts where they are and sends only what the server keeps
//! anyway (calls with their token counts, what sessions and subagents told each other, Remote
//! Control links), never the files. The server keeps how far it has each file, so a repeated or
//! missed sync sends nothing twice and nothing is lost.

use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::{AppState, json_error};
use crate::auth::Actor;
use crate::usage::Read;

/// A round of one transcript is bounded by flow; this leaves room for a long one.
const MAX_UPLOAD: usize = 64 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/sync/state", post(state_of))
        .route("/api/sync", post(upload).layer(DefaultBodyLimit::max(MAX_UPLOAD)))
}

/// The key a file's offset is kept under: the project's own, so one project's token can never
/// move another's.
fn key(slug: &str, file: &str) -> anyhow::Result<String> {
    let file = file.trim();
    if file.is_empty() || file.len() > 1000 || file.chars().any(char::is_control) {
        anyhow::bail!("name the file as machine:path");
    }
    Ok(format!("sync:{slug}:{file}"))
}

fn project_of(actor: &Actor) -> Option<(i64, String)> {
    match actor {
        Actor::Project { project_id, slug, .. } => Some((*project_id, slug.clone())),
        _ => None,
    }
}

#[derive(Deserialize)]
struct StateBody {
    file: String,
}

/// Where the last sync of this file stopped.
async fn state_of(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(b): Json<StateBody>,
) -> Response {
    let Some((_, slug)) = project_of(&actor) else {
        return json_error(StatusCode::FORBIDDEN, "flow sync runs with a project's token");
    };
    let key = match key(&slug, &b.file) {
        Ok(k) => k,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    match state.db.call(move |c| crate::usage::file_state(c, std::path::Path::new(&key))).await {
        Ok((offset, first_at)) => Json(json!({"offset": offset, "first_at": first_at})).into_response(),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct UploadBody {
    file: String,
    /// The offset this round started from: it must be where the server stopped, so rounds sent
    /// twice or out of order are refused instead of stored twice.
    from: u64,
    read: Read,
}

async fn upload(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(b): Json<UploadBody>,
) -> Response {
    let Some((project_id, slug)) = project_of(&actor) else {
        return json_error(StatusCode::FORBIDDEN, "flow sync runs with a project's token");
    };
    let key = match key(&slug, &b.file) {
        Ok(k) => k,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    let mut read = b.read;
    read.path = key.clone();
    let from = b.from;
    let stored = state
        .db
        .call(move |c| {
            let (offset, _) = crate::usage::file_state(c, std::path::Path::new(&key))?;
            // From 0 again: the file was rewritten from the start; the calls' ids keep anything
            // from counting twice.
            if offset != from && from != 0 {
                return Ok(Err(offset));
            }
            if read.offset < from {
                anyhow::bail!("a round can't end before it starts");
            }
            read.confine(c, project_id)?;
            let calls = crate::usage::store_as(c, &read, Some(project_id))?;
            crate::projects::seen(c, project_id)?;
            Ok(Ok((calls, read.offset)))
        })
        .await;
    match stored {
        Ok(Ok((calls, offset))) => Json(json!({"calls": calls, "offset": offset})).into_response(),
        Ok(Err(offset)) => (
            StatusCode::CONFLICT,
            Json(json!({"error": "the server is elsewhere in this file; ask for its offset again", "offset": offset})),
        )
            .into_response(),
        Err(e) => json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}
