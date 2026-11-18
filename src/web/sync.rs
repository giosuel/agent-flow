//! `flow sync` (the owner, 2026-11-13: decouple AgentFlow from the machine it runs on): a
//! project's hooks send what's new in its Claude Code conversations after each reply, subagent and
//! session end. flow reads the transcripts where they are and sends only what the server keeps
//! anyway (calls with their token counts, what sessions and subagents told each other, Remote
//! Control links), never the files. The server keeps how far it has each file, so a repeated or
//! missed sync sends nothing twice and nothing is lost.

use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
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
        .route("/api/sync/split", post(split).layer(DefaultBodyLimit::max(MAX_UPLOAD)))
        .route("/api/usage/plan", post(plan))
        .route("/api/usage", get(usage_api))
}

#[derive(Deserialize)]
struct UsageQuery {
    all: Option<u8>,
}

/// `flow usage`: the plan's windows and the project's tokens and cost (every project's with
/// `all=1`, and always for the owner's own machines).
async fn usage_api(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    axum::extract::Query(q): axum::extract::Query<UsageQuery>,
) -> Response {
    let project = match &actor {
        Actor::Project { project_id, .. } if q.all != Some(1) => Some(*project_id),
        Actor::Project { .. } | Actor::Owner { .. } | Actor::Device { .. } | Actor::Admin { .. } => None,
        _ => return json_error(StatusCode::FORBIDDEN, "not for this token"),
    };
    let now = crate::util::now_ms();
    match state.db.call(move |c| crate::usage_report::report(c, project, now, &crate::tz::Tz::default())).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct PlanBody {
    rate_limits: crate::plan::Limits,
    #[serde(default)]
    machine: String,
}

/// The plan's windows from a session's status line (`flow statusline`, M13); at most one a minute
/// is kept, whichever project sends it.
async fn plan(State(state): State<AppState>, Extension(actor): Extension<Actor>, Json(b): Json<PlanBody>) -> Response {
    let Some((project_id, _)) = project_of(&actor) else {
        return json_error(StatusCode::FORBIDDEN, "flow statusline runs with a project's token");
    };
    let now = crate::util::now_ms();
    match state.db.call(move |c| crate::plan::store(c, &b.rate_limits, now, Some(project_id), &b.machine)).await {
        Ok(stored) => {
            let tz = crate::tz::Tz::default();
            for x in &stored.crossed {
                let (what, when) =
                    if x.five { ("The 5-hour window", tz.hm(x.reset)) } else { ("The week", tz.short(x.reset)) };
                state.push.spawn(
                    crate::push::Kind::Usage,
                    crate::push::Notification {
                        title: format!("{what} passed 80%"),
                        body: format!("{:.0}% of the plan used; it resets {when}.", x.pct),
                        url: "/usage".into(),
                        tag: Some(format!("plan-{}", if x.five { "5h" } else { "week" })),
                    },
                );
            }
            Json(json!({"kept": stored.kept})).into_response()
        }
        Err(e) => json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct SplitBody {
    /// (message id, cache writes kept for an hour, fast mode)
    calls: Vec<(String, i64, bool)>,
}

/// Cache writes by duration and fast mode for calls AgentFlow stored before it kept them (M13):
/// flow sends them once per folder; only the project's own calls change.
async fn split(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(b): Json<SplitBody>,
) -> Response {
    let Some((project_id, _)) = project_of(&actor) else {
        return json_error(StatusCode::FORBIDDEN, "flow sync runs with a project's token");
    };
    if b.calls.len() > 100_000 {
        return json_error(StatusCode::BAD_REQUEST, "at most 100000 calls at once");
    }
    match state.db.call(move |c| crate::usage::fill_split(c, project_id, &b.calls)).await {
        Ok(n) => Json(json!({"filled": n})).into_response(),
        Err(e) => json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
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
    let messages = read.messages();
    let file = b.file.rsplit('/').next().unwrap_or("").to_owned();
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
        Ok(Ok((calls, offset))) => {
            super::LAST_READ.store(crate::util::now_ms(), std::sync::atomic::Ordering::Relaxed);
            // One entry for a round that brought something, none for an empty one.
            if calls > 0 || messages > 0 {
                let what = match (calls, messages) {
                    (c, 0) => format!("{c} call{}", if c == 1 { "" } else { "s" }),
                    (0, m) => format!("{m} message{}", if m == 1 { "" } else { "s" }),
                    (c, m) => format!("{c} calls, {m} messages"),
                };
                let e = crate::activity::Entry::new(
                    &actor,
                    crate::activity::Via::Api,
                    crate::activity::Action::Write,
                    format!("flow sync: {what} from {file}"),
                )
                .project(slug.clone());
                crate::activity::write(&state.db, e).await;
            }
            Json(json!({"calls": calls, "offset": offset})).into_response()
        }
        Ok(Err(offset)) => (
            StatusCode::CONFLICT,
            Json(json!({"error": "the server is elsewhere in this file; ask for its offset again", "offset": offset})),
        )
            .into_response(),
        Err(e) => json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}
