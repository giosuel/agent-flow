//! The Conversations page: what agents and the consultant tell each other, and the owner's replies.

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Form, Router};
use serde::Deserialize;

use super::pages::{Nav, nav, render};
use super::{AppError, AppResult, AppState, ClientIp};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::messages::{self, OWNER};
use crate::util::{ago, fmt_short, now_ms};
use crate::{markdown, records};

pub fn routes() -> Router<AppState> {
    Router::new().route("/conversations", get(page)).route("/conversations/{id}/reply", post(reply))
}

struct ThreadRow {
    id: i64,
    subject: String,
    members: String,
    when: String,
    kind: String,
    count: i64,
    selected: bool,
}

struct MessageView {
    sender: String,
    at: String,
    kind: String,
    body_html: String,
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
    threads: Vec<ThreadRow>,
    selected: Option<(i64, String, String)>,
    messages: Vec<MessageView>,
    contributions: Vec<ContributionRow>,
    sent: bool,
}

#[derive(Deserialize)]
struct PageQuery {
    t: Option<i64>,
    sent: Option<u8>,
}

/// "Vinculum ↔ Antimony", "Cold Trace → everyone"
fn members_line(started_by: &str, members: &[String]) -> String {
    let others: Vec<&str> = members.iter().map(String::as_str).filter(|m| *m != started_by).collect();
    let others = others.iter().map(|m| if *m == messages::ALL { "everyone" } else { m }).collect::<Vec<_>>().join(", ");
    format!("{started_by} → {others}")
}

async fn page(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<PageQuery>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let now = now_ms();
    let (threads, selected, msgs, contributions) = state
        .db
        .call(move |c| {
            let threads = messages::threads(c, None, 100, None)?;
            let pick = q.t.or(threads.first().map(|t| t.id));
            let selected = match pick {
                Some(id) => messages::thread(c, id)?,
                None => None,
            };
            let msgs = match &selected {
                Some(t) => messages::messages(c, t.id)?,
                None => Vec::new(),
            };
            let contributions = records::query(
                c,
                &records::Query { kinds: vec![records::Kind::Contribution], limit: 50, ..Default::default() },
            )?;
            Ok((threads, selected, msgs, contributions))
        })
        .await?;
    let sel_id = selected.as_ref().map(|t| t.id);
    render(ConversationsPage {
        nav: nav(&state, &actor, "/conversations").await,
        threads: threads
            .iter()
            .map(|t| ThreadRow {
                id: t.id,
                subject: t.subject.clone(),
                members: members_line(&t.started_by, &t.members),
                when: ago(t.last_at, now),
                kind: t.last_kind.clone(),
                count: t.count,
                selected: Some(t.id) == sel_id,
            })
            .collect(),
        selected: selected.map(|t| {
            (
                t.id,
                t.subject.clone(),
                format!("{} · started {}", members_line(&t.started_by, &t.members), fmt_short(t.created_at)),
            )
        }),
        messages: msgs
            .iter()
            .map(|m| MessageView {
                sender: if m.sender == OWNER { "You".into() } else { m.sender.clone() },
                at: fmt_short(m.created_at),
                kind: m.kind.clone(),
                body_html: markdown::to_html(&m.body),
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
        sent: q.sent == Some(1),
    })
}

#[derive(Deserialize)]
struct ReplyForm {
    body: String,
}

async fn reply(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Path(id): Path<i64>,
    Form(form): Form<ReplyForm>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let body = form.body;
    state
        .db
        .call(move |c| {
            let tx = c.transaction()?;
            messages::send(&tx, OWNER, &[], "", &body, "info", Some(id))?;
            tx.commit()?;
            Ok(())
        })
        .await
        .map_err(|e| AppError::BadRequest(format!("{e:#}")))?;
    let e = Entry::new(&actor, Via::Page, Action::Message, format!("wrote in conversation {id}")).ip(Some(ip.0));
    activity::write(&state.db, e).await;
    Ok(Redirect::to(&format!("/conversations?t={id}&sent=1")).into_response())
}
