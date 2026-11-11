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
    Router::new().route("/api/wait", get(wait))
}

#[derive(Deserialize)]
struct WaitQuery {
    /// Seconds to wait at most (capped at 25 minutes; the CLI asks again).
    timeout: Option<u64>,
    /// Only return once this record has an event, e.g. "Q-12".
    record: Option<String>,
}

const POLL: Duration = Duration::from_secs(2);
const MAX_WAIT: u64 = 25 * 60;

async fn wait(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<WaitQuery>,
) -> Response {
    let Actor::Project { project_id, slug, .. } = actor.clone() else {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "wait is for a project's agent token"}))).into_response();
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
