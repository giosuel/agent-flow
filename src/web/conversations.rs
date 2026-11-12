//! The Conversations page: what sessions tell each other with Claude Code's SendMessage, and what
//! each session's subagents were told and reported, read from the transcripts (crate::sessionlog;
//! the owner, 2026-11-12: "keep the claude code sendmessage and use agent flow only for logging").
//! The owner reads along; nobody writes here.

use askama::Template;
use axum::extract::{Query, State};
use axum::response::Response;
use axum::routing::get;
use axum::{Extension, Router};
use serde::Deserialize;

use super::pages::{Nav, nav, render};
use super::{AppResult, AppState};
use crate::auth::Actor;
use crate::sessionlog;
use crate::tz::Tz;
use crate::util::{ago, now_ms};
use crate::{markdown, records};

pub fn routes() -> Router<AppState> {
    Router::new().route("/conversations", get(page))
}

struct ThreadRow {
    key: String,
    title: String,
    sub: String,
    count: i64,
    when: String,
    selected: bool,
}

struct MessageView {
    from: String,
    to: String,
    at: String,
    body_html: String,
    /// A long message folds to its first lines.
    preview: Option<String>,
}

/// Messages longer than this fold to a preview (briefs and reports run to pages).
const FOLD_AT: usize = 1200;

fn preview(body: &str) -> Option<String> {
    if body.len() <= FOLD_AT {
        return None;
    }
    let flat: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(flat.chars().take(300).collect::<String>() + "…")
}

struct ContributionRow {
    title: String,
    url: String,
    state: String,
    project: String,
}

#[derive(Template)]
#[template(path = "conversations.html")]
struct ConversationsPage {
    nav: Nav,
    sessions: Vec<ThreadRow>,
    subagents: Vec<ThreadRow>,
    /// The conversation shown: (title, line).
    selected: Option<(String, String)>,
    messages: Vec<MessageView>,
    contributions: Vec<ContributionRow>,
    /// A conversation was chosen (not just the newest shown): on the phone, the list steps aside.
    picked: bool,
    reading: bool,
    /// The query that brings this page back (for the Reload button).
    here: String,
    /// Every subagent, not just the newest.
    all: bool,
}

#[derive(Deserialize)]
struct PageQuery {
    t: Option<String>,
    all: Option<String>,
}

async fn page(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<PageQuery>,
    tz: Tz,
) -> AppResult<Response> {
    actor.require_owner()?;
    let now = now_ms();
    let picked = q.t.is_some();
    let all = q.all.is_some();
    let pick = q.t.clone();
    let (sessions, subagents, msgs, contributions, names) = state
        .db
        .call(move |c| {
            let names: std::collections::HashMap<i64, String> =
                crate::projects::list(c)?.into_iter().map(|p| (p.id, p.name)).collect();
            let sessions = sessionlog::session_threads(c, 200)?;
            let subagents = sessionlog::subagent_threads(c, None, if all { 2000 } else { 40 })?;
            let pick = pick.or_else(|| {
                let a = sessions.first().map(|t| (t.last_at, t.key.clone()));
                let b = subagents.first().map(|t| (t.last_at, t.key.clone()));
                a.into_iter().chain(b).max().map(|(_, k)| k)
            });
            let msgs = match &pick {
                Some(k) => sessionlog::messages(c, k)?,
                None => Vec::new(),
            };
            let contributions = records::query(
                c,
                &records::Query { kinds: vec![records::Kind::Contribution], limit: 50, ..Default::default() },
            )?;
            Ok((sessions, subagents, (pick, msgs), contributions, names))
        })
        .await?;
    let (pick, msgs) = msgs;
    let row = |t: &sessionlog::Thread| ThreadRow {
        key: t.key.clone(),
        title: t.title.clone(),
        sub: [t.sub.clone(), t.project_id.and_then(|p| names.get(&p).cloned()).filter(|_| !t.is_subagent).unwrap_or_default()]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · "),
        count: t.count,
        when: ago(t.last_at, now),
        selected: pick.as_deref() == Some(t.key.as_str()),
    };
    let selected = pick.as_ref().and_then(|k| {
        sessions.iter().chain(subagents.iter()).find(|t| &t.key == k).map(|t| {
            let line = if t.is_subagent {
                format!("Subagent{} · started {}", if t.sub.is_empty() { String::new() } else { format!(" · {}", t.sub) }, msgs.first().map(|m| tz.short(m.at)).unwrap_or_default())
            } else {
                format!("Between sessions · {} message{}", t.count, if t.count == 1 { "" } else { "s" })
            };
            (t.title.clone(), line)
        })
    });
    let here = match &pick {
        Some(k) => format!("/conversations?t={}", url_encode(k)),
        None => "/conversations".into(),
    };
    render(ConversationsPage {
        nav: nav(&state, &actor, "/conversations").await,
        sessions: sessions.iter().map(row).collect(),
        subagents: subagents.iter().map(row).collect(),
        selected,
        messages: msgs
            .iter()
            .map(|m| MessageView {
                from: sessionlog::shown(&m.from),
                to: sessionlog::shown(&m.to),
                at: tz.short(m.at),
                body_html: markdown::to_html(&m.body),
                preview: preview(&m.body),
            })
            .collect(),
        contributions: contributions
            .iter()
            .map(|r| ContributionRow {
                title: r.title.clone(),
                url: r.str("url").to_owned(),
                state: r.state.clone(),
                project: r.project.clone(),
            })
            .collect(),
        picked,
        reading: super::READING.load(std::sync::atomic::Ordering::Relaxed),
        here,
        all,
    })
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
