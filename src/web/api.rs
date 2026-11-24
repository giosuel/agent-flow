//! JSON API for the CLI: `agentflow wait` blocks here until something happens in the caller's
//! project. Agents run it in the background, so an idle session is woken when the owner answers.

use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::auth::Actor;
use crate::util::fmt_utc;
use crate::{records, waiting};

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/wait", get(wait)).route("/api/news", get(news)).route("/api/hello", axum::routing::post(hello)).route("/api/timings", axum::routing::post(timing))
}

/// A timing from `agentflow time`: what ran, how long, CPU and peak memory, on what.
#[derive(Deserialize)]
pub struct TimingBody {
    pub what: String,
    pub wall_ms: i64,
    pub cpu_ms: Option<i64>,
    pub peak_mb: Option<i64>,
    pub exit_code: Option<i64>,
    #[serde(default)]
    pub on: String,
    #[serde(default)]
    pub place: String,
    #[serde(default)]
    pub note: String,
}

async fn timing(State(state): State<AppState>, Extension(actor): Extension<Actor>, Json(b): Json<TimingBody>) -> Response {
    let a = actor.clone();
    let res = state
        .db
        .call(move |c| {
            let project = crate::mcp::target_project(c, &a, None, true)?;
            let on = crate::times::resolve_on(c, project.id, &project.slug, Some(b.on.as_str()).filter(|o| !o.is_empty()))?;
            let place = if b.place.trim().is_empty() { "here" } else { b.place.as_str() };
            crate::times::add(
                c,
                project.id,
                &on,
                &crate::times::NewTiming { what: &b.what, place, wall_ms: b.wall_ms, cpu_ms: b.cpu_ms, peak_mb: b.peak_mb, exit_code: b.exit_code, note: &b.note },
                &a.name(),
            )
        })
        .await;
    match res {
        Ok(t) => {
            let e = crate::activity::Entry::new(&actor, crate::activity::Via::Api, crate::activity::Action::Write, format!("timing on {}: {}", t.on, t.what));
            crate::activity::write(&state.db, e).await;
            Json(json!({"on": t.on, "id": t.id})).into_response()
        }
        Err(e) => super::json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct NewsQuery {
    /// See the last this-many minutes again, read or not (the bookmark stays).
    again: Option<i64>,
}

/// The news for `agentflow news` (a hook before each of the owner's prompts): unread events by
/// others, marked read; or, with `again`, the last minutes once more.
async fn news(State(state): State<AppState>, Extension(actor): Extension<Actor>, Query(q): Query<NewsQuery>) -> Response {
    let Actor::Project { project_id, slug, .. } = actor else {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "news is for a project's agent"}))).into_response();
    };
    let result = state
        .db
        .call(move |c| match q.again {
            Some(min) if min > 0 => waiting::news_again(c, project_id, &slug, min * 60_000),
            _ => waiting::read_news(c, project_id, &slug),
        })
        .await;
    match result {
        Ok(events) => Json(json!({"events": events.iter().map(waiting::news_json).collect::<Vec<_>>()})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("{e:#}")}))).into_response(),
    }
}

/// Who is saying hello, from `agentflow hello` (a SessionStart hook); what's left empty keeps the
/// project's last values.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct HelloBody {
    pub session: String,
    pub machine: String,
    pub model: String,
    pub effort: String,
    /// Why the session (re)started: startup, resume, clear, compact or fork.
    pub source: String,
    /// "doc": only the project document, with no side effects (the second of two hooks).
    pub part: String,
}

/// `agentflow hello`: the hello tool's reply, for a SessionStart hook (Imaginarium's agent and the
/// owner, 2026-11-12: the full hello on every start, resume, /clear and compaction). The news in it
/// is marked read, so the hook before the next prompt doesn't repeat it.
async fn hello(State(state): State<AppState>, Extension(actor): Extension<Actor>, Json(b): Json<HelloBody>) -> Response {
    if !matches!(actor, Actor::Project { .. }) {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "hello is for a project's agent"}))).into_response();
    }
    let token_dir = state.config.token_dir();
    let a = actor.clone();
    let source = b.source.trim().to_owned();
    let res = state
        .db
        .call(move |c| {
            let project = crate::mcp::target_project(c, &a, None, true)?;
            if b.part == "doc" {
                let d = crate::projectdoc::get(c, project.id)?;
                let edited = if d.rev > 0 { format!("{} by {} ({})", crate::util::fmt_utc(d.updated_at), d.author, d.what) } else { String::new() };
                return Ok((json!({"doc": {"text": d.text, "rev": d.rev, "edited": edited}}), None, project.slug));
            }
            let keep = |new: String, old: &str| if new.trim().is_empty() { old.to_owned() } else { new };
            let p = crate::mcp::Hello {
                session: keep(b.session, &project.agent.session),
                machine: keep(b.machine, &project.agent.machine),
                model: keep(b.model, &project.agent.model),
                effort: keep(b.effort, &project.agent.effort),
                brief: false,
            };
            let mut reply = crate::mcp::hello_reply(c, &project, &p, &token_dir)?;
            let news = waiting::read_news(c, project.id, &project.slug)?;
            reply["news"] = news.iter().map(waiting::news_json).collect::<Vec<_>>().into();
            Ok((reply, Some(p.session), project.slug))
        })
        .await;
    match res {
        Ok((reply, None, _)) => Json(reply).into_response(),
        Ok((reply, Some(session), slug)) => {
            let how = if source.is_empty() { String::new() } else { format!(" ({source})") };
            let mut e = crate::activity::Entry::new(&actor, crate::activity::Via::Api, crate::activity::Action::Read, format!("hello from {session}{how}"));
            e.project = Some(slug);
            crate::activity::write(&state.db, e).await;
            Json(reply).into_response()
        }
        Err(e) => super::json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    }
}

#[derive(Deserialize)]
struct WaitQuery {
    /// Seconds to wait at most (capped at 25 minutes; the CLI asks again).
    timeout: Option<u64>,
    /// Only return once this record has an event, e.g. "Q-12".
    record: Option<String>,
}

const POLL: Duration = Duration::from_secs(2);
const MAX_WAITS_PER_PROJECT: u32 = 2;

/// A running wait, counted per project and released when the request ends (even if the client
/// goes away mid-wait).
struct WaitSlot {
    state: AppState,
    project_id: i64,
}

impl WaitSlot {
    fn take(state: &AppState, project_id: i64) -> Option<Self> {
        let mut map = crate::util::lock(&state.waits);
        let n = map.entry(project_id).or_insert(0);
        if *n >= MAX_WAITS_PER_PROJECT {
            return None;
        }
        *n += 1;
        Some(Self { state: state.clone(), project_id })
    }
}

impl Drop for WaitSlot {
    fn drop(&mut self) {
        if let Some(n) = crate::util::lock(&self.state.waits).get_mut(&self.project_id) {
            *n = n.saturating_sub(1);
        }
    }
}
const MAX_WAIT: u64 = 25 * 60;

async fn wait(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<WaitQuery>,
) -> Response {
    let Actor::Project { project_id, slug, .. } = actor.clone() else {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "wait is for a project's agent"})))
            .into_response();
    };
    // One agent per project needs one or two waits at a time; more would only load the database.
    let Some(_slot) = WaitSlot::take(&state, project_id) else {
        return (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error": "this project already has two waits running"})))
            .into_response();
    };
    let record_ref = match q.record.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) => {
            let (r, s) = (r.to_owned(), slug.clone());
            match state.db.call(move |c| records::find_str(c, &r, Some(&s))).await {
                Ok(rec) => Some(rec.ref_id),
                Err(e) => return (StatusCode::NOT_FOUND, Json(json!({"error": format!("{e:#}")}))).into_response(),
            }
        }
        None => None,
    };
    let deadline = Instant::now() + Duration::from_secs(q.timeout.unwrap_or(MAX_WAIT).clamp(1, MAX_WAIT));
    loop {
        let (s, r) = (slug.clone(), record_ref.clone());
        let found = state
            .db
            .call(move |c| {
                let mut events = waiting::unread(c, project_id, &s)?;
                if let Some(r) = &r
                    && !events.iter().any(|e| e.record.as_deref() == Some(r))
                {
                    events.clear();
                }
                if let Some(last) = events.last() {
                    waiting::set_cursor(c, project_id, last.seq)?;
                }
                Ok(events)
            })
            .await;
        match found {
            Ok(events) if !events.is_empty() => {
                let list: Vec<_> = events
                    .iter()
                    .map(|e| json!({"at": fmt_utc(e.at), "record": e.record, "kind": e.kind, "what": e.summary, "by": e.actor}))
                    .collect();
                return Json(json!({"events": list})).into_response();
            }
            Ok(_) => {}
            Err(e) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("{e:#}")}))).into_response();
            }
        }
        if Instant::now() >= deadline {
            return Json(json!({"events": [], "timed_out": true})).into_response();
        }
        tokio::time::sleep(POLL).await;
    }
}

