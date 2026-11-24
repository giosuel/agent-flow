//! Drafts for other people's repositories over HTTP: agents send them (`flow contribute`), the
//! owner sends, sends back or drops them from the contribution's page. See `crate::outgoing`.

use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Form, Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::{AppError, AppResult, AppState, ClientIp, json_error};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::outgoing::{self, DraftIn, Env};
use crate::records::{self, Kind, RecordRef};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/contributions",
            // A bundle of commits, base64, with the text.
            post(draft).layer(DefaultBodyLimit::max(outgoing::MAX_BUNDLE * 4 / 3 + 1024 * 1024)),
        )
        .route("/api/contributions/{slug}/{id}", get(about))
        .route("/p/{slug}/{id}/send", post(send))
        .route("/p/{slug}/{id}/changes", post(changes))
        .route("/p/{slug}/{id}/drop", post(drop_it))
        .route("/p/{slug}/{id}/diff", get(diff))
}

fn env(state: &AppState) -> (std::path::PathBuf, String) {
    (state.config.data_dir(), state.config.git_host.clone())
}

/// An agent (or the consultant) sends a draft or a new version of one.
async fn draft(State(state): State<AppState>, Extension(actor): Extension<Actor>, Json(d): Json<DraftIn>) -> Response {
    if !matches!(actor, Actor::Project { .. } | Actor::Admin { .. }) {
        return json_error(StatusCode::FORBIDDEN, "only project agents and the consultant draft contributions");
    }
    let (data_dir, git_host) = env(&state);
    let e = Env { data_dir: &data_dir, git_host: &git_host };
    let r = match outgoing::submit(&state.db, &e, &actor, d).await {
        Ok(r) => r,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    let entry =
        Entry::new(&actor, Via::Api, Action::Write, format!("contribution draft {}: {}", r.short_id(), r.title))
            .project(r.project.clone())
            .target(r.ref_id.clone());
    activity::write(&state.db, entry).await;
    let version = tell_owner(&state, &r);
    let url = format!("{}{}", state.config.public_url.trim_end_matches('/'), r.url());
    Json(json!({"id": r.ref_id, "version": version, "url": url, "state": "waiting for the owner"})).into_response()
}

/// A notification for a new draft or a new version of one; returns the version.
pub fn tell_owner(state: &AppState, r: &records::Record) -> i64 {
    let p = outgoing::pending(r);
    let what = p.as_ref().map(|p| p.kind).map(|k| match k {
        outgoing::DraftType::Pr => "A PR",
        outgoing::DraftType::Issue => "An issue",
        outgoing::DraftType::Comment => "A comment",
        outgoing::DraftType::Commits => "New commits",
    });
    let version = p.as_ref().map(|p| p.version).unwrap_or(1);
    state.push.spawn(
        crate::push::Kind::Review,
        crate::push::Notification {
            title: format!("{} for {} to send", what.unwrap_or("A draft"), r.str("repo")),
            body: format!(
                "{}{}: {}",
                r.project,
                if version > 1 { format!(", version {version}") } else { String::new() },
                r.title
            ),
            url: r.url(),
            tag: Some(format!("draft-{}", r.ref_id)),
        },
    );
    version
}

/// What `flow contribute commits` needs to know about a contribution.
async fn about(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((slug, id)): Path<(String, String)>,
) -> Response {
    if matches!(actor, Actor::Anonymous) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let found = state
        .db
        .call(move |c| {
            let Ok(rref) = RecordRef::parse(&format!("{slug}/{id}")) else { return Ok(None) };
            Ok(records::try_find(c, &rref, None)?.filter(|r| r.kind == Kind::Contribution))
        })
        .await;
    let Ok(Some(r)) = found else { return json_error(StatusCode::NOT_FOUND, "no such contribution") };
    Json(json!({
        "id": r.ref_id,
        "state": r.state,
        "type": r.str("type"),
        "repo": r.str("repo"),
        "number": r.data.get("number"),
        "url": r.str("url"),
        "fork": r.str("fork"),
        "branch": r.str("branch"),
        "base": r.str("base"),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct DraftForm {
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    note: String,
    /// The draft's fingerprint as the page showed it.
    #[serde(default)]
    fp: String,
}

async fn find(state: &AppState, slug: String, id: String) -> AppResult<records::Record> {
    state
        .db
        .call(move |c| {
            let Ok(rref) = RecordRef::parse(&format!("{slug}/{id}")) else { return Ok(None) };
            Ok(records::try_find(c, &rref, None)?.filter(|r| r.kind == Kind::Contribution))
        })
        .await?
        .ok_or(AppError::NotFound)
}

fn back(r: &records::Record, outcome: &str) -> Response {
    Redirect::to(&format!("{}?draft={outcome}", r.url())).into_response()
}

fn log(actor: &Actor, ip: &ClientIp, r: &records::Record, what: String) -> Entry {
    Entry::new(actor, Via::Page, Action::Write, what)
        .project(r.project.clone())
        .target(r.ref_id.clone())
        .ip(Some(ip.0.clone()))
}

/// The owner sends it: pushed and posted as them, with the text as they left it.
async fn send(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Path((slug, id)): Path<(String, String)>,
    Form(f): Form<DraftForm>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let r = find(&state, slug, id).await?;
    let (data_dir, git_host) = env(&state);
    let e = Env { data_dir: &data_dir, git_host: &git_host };
    match outgoing::send(&state.db, &e, state.poster.as_ref(), r.id, f.title, f.body, f.fp).await {
        Ok(url) => {
            activity::write(&state.db, log(&actor, &ip, &r, format!("sent {}: {url}", r.short_id()))).await;
            Ok(back(&r, "sent"))
        }
        Err(err) => {
            tracing::warn!("sending {}: {err:#}", r.ref_id);
            activity::write(&state.db, log(&actor, &ip, &r, format!("sending {} failed: {err:#}", r.short_id()))).await;
            Ok(back(&r, if format!("{err:#}").contains("changed since") { "changed" } else { "failed" }))
        }
    }
}

/// The owner sends it back to the agent with a note (and their edits).
async fn changes(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Path((slug, id)): Path<(String, String)>,
    Form(f): Form<DraftForm>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let r = find(&state, slug, id).await?;
    if f.note.trim().is_empty() {
        return Ok(back(&r, "nonote"));
    }
    let (rid, fp) = (r.id, f.fp.clone());
    let done = state
        .db
        .call(move |c| {
            let r = records::get(c, rid)?;
            if outgoing::fingerprint(&r) != fp {
                anyhow::bail!("changed");
            }
            outgoing::ask_changes(c, &r, &f.title, &f.body, &f.note)
        })
        .await;
    match done {
        Ok(_) => {
            activity::write(&state.db, log(&actor, &ip, &r, format!("asked for changes to {}", r.short_id()))).await;
            Ok(back(&r, "asked"))
        }
        Err(e) if format!("{e:#}") == "changed" => Ok(back(&r, "changed")),
        Err(e) => {
            tracing::warn!("asking for changes to {}: {e:#}", r.ref_id);
            Ok(back(&r, "failed"))
        }
    }
}

/// The owner drops it: nothing is sent.
async fn drop_it(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Path((slug, id)): Path<(String, String)>,
    Form(f): Form<DraftForm>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let r = find(&state, slug, id).await?;
    let rid = r.id;
    let done = state.db.call(move |c| outgoing::drop_draft(c, &records::get(c, rid)?, &f.note)).await;
    match done {
        Ok(_) => {
            activity::write(&state.db, log(&actor, &ip, &r, format!("dropped the draft {}", r.short_id()))).await;
            Ok(back(&r, "dropped"))
        }
        Err(e) => {
            tracing::warn!("dropping {}: {e:#}", r.ref_id);
            Ok(back(&r, "failed"))
        }
    }
}

/// A draft as its page shows it.
pub struct DraftView {
    pub heading: String,
    pub kind: &'static str,
    /// Waiting for the owner: the form works.
    pub waiting: bool,
    pub sending: bool,
    /// Back with the agent after "Ask for changes".
    pub with_agent: bool,
    /// A new PR or issue has a title of its own.
    pub has_title: bool,
    pub title: String,
    pub body: String,
    pub fp: String,
    pub target: String,
    pub drafted: String,
    pub checks: Vec<outgoing::Check>,
    pub commits: Vec<(String, String, String)>,
    pub stat: String,
    /// (class, text) per line: fh (a file), h (a hunk), a (added), d (removed), or "".
    pub diff: Vec<(&'static str, String)>,
    /// Lines left out of the page (the whole diff is one link away).
    pub diff_more: usize,
    pub why: String,
    pub task: Option<(String, String)>,
    pub template: Option<(String, String)>,
    pub contributing: Option<(String, String)>,
    /// The owner's last "Ask for changes" note.
    pub note: String,
    pub send_label: &'static str,
    pub on_send: String,
}

/// Most diff lines shown on the page.
const DIFF_LINES: usize = 3000;

pub fn draft_view(c: &rusqlite::Connection, r: &records::Record, now: i64) -> anyhow::Result<Option<DraftView>> {
    let Some(p) = outgoing::pending(r) else { return Ok(None) };
    let s = |k: &str| p.data.get(k).and_then(serde_json::Value::as_str).unwrap_or("").to_owned();
    let code: Option<outgoing::Code> = p.data.get("code").and_then(|c| serde_json::from_value(c.clone()).ok());
    let repo = r.str("repo").to_owned();
    let number = r.data.get("number").and_then(serde_json::Value::as_i64).unwrap_or(0);
    let target = match p.kind {
        outgoing::DraftType::Pr => format!("{repo} · {} ← your fork's {}", r.str("base"), r.str("branch")),
        outgoing::DraftType::Issue => repo.clone(),
        _ => format!("{repo} #{number}"),
    };
    let drafted_at = p.data.get("drafted_at").and_then(serde_json::Value::as_i64).unwrap_or(r.updated_at);
    let drafted = format!(
        "drafted by {} {}{}",
        s("drafted_by"),
        crate::util::ago(drafted_at, now),
        if p.version > 1 { format!(" · version {}", p.version) } else { String::new() }
    );
    let checks = outgoing::checks(c, r, &p)?;
    let (mut diff, mut diff_more) = (Vec::new(), 0);
    if let Some(text) = outgoing::diff_text(&p) {
        for (i, line) in text.lines().enumerate() {
            if i >= DIFF_LINES {
                diff_more += 1;
                continue;
            }
            let class = if line.starts_with("diff --git") {
                "fh"
            } else if line.starts_with("+++") || line.starts_with("---") || line.starts_with("index ") {
                "meta"
            } else if line.starts_with("@@") {
                "h"
            } else if line.starts_with('+') {
                "a"
            } else if line.starts_with('-') {
                "d"
            } else {
                ""
            };
            diff.push((class, line.to_owned()));
        }
    }
    let file = |k: &str| {
        let v = r.data.get(k)?;
        Some((v.get("name")?.as_str()?.to_owned(), v.get("text")?.as_str()?.to_owned()))
    };
    let task = match s("task").as_str() {
        "" => None,
        t => records::try_find(c, &RecordRef::parse(t)?, Some(&r.project))?.map(|t| (t.ref_id.clone(), t.url())),
    };
    let (send_label, on_send) = match p.kind {
        outgoing::DraftType::Pr => (
            "Send the PR",
            format!(
                "Send pushes the commits to your fork of {repo} (made if you don't have one) and opens the PR there as you, \
                 with exactly this title and text."
            ),
        ),
        outgoing::DraftType::Issue => {
            ("Open the issue", format!("Send opens the issue on {repo} as you, with exactly this title and text."))
        }
        outgoing::DraftType::Comment => {
            ("Post the comment", format!("Send posts the comment on {repo} #{number} as you, word for word."))
        }
        outgoing::DraftType::Commits => (
            "Push the commits",
            format!(
                "Send pushes the commits on top of the PR's branch on your fork (never forced){}.",
                if p.body.trim().is_empty() { "" } else { ", then posts the note as a comment" }
            ),
        ),
    };
    Ok(Some(DraftView {
        heading: match p.kind {
            outgoing::DraftType::Pr => format!("New PR to {repo}"),
            outgoing::DraftType::Issue => format!("New issue for {repo}"),
            outgoing::DraftType::Comment => format!("Comment on {repo} #{number}"),
            outgoing::DraftType::Commits => format!("New commits for {repo} #{number}"),
        },
        kind: p.kind.as_str(),
        waiting: p.state == "shown",
        sending: p.state == "sending",
        with_agent: p.state == "draft",
        has_title: p.kind.is_new(),
        title: p.title.to_owned(),
        body: p.body.to_owned(),
        fp: outgoing::fingerprint(r),
        target,
        drafted,
        checks,
        commits: code
            .as_ref()
            .map(|c| {
                c.commits
                    .iter()
                    .map(|k| (k.sha.get(..7).unwrap_or("").to_owned(), k.subject.clone(), k.author.clone()))
                    .collect()
            })
            .unwrap_or_default(),
        stat: code.as_ref().map(|c| c.stat.clone()).unwrap_or_default(),
        diff,
        diff_more,
        why: s("why"),
        task,
        template: if p.kind == outgoing::DraftType::Pr { file("template") } else { None },
        contributing: if p.kind == outgoing::DraftType::Pr { file("contributing") } else { None },
        note: s("note"),
        send_label,
        on_send,
    }))
}

/// The message after one of the three buttons, from `?draft=`.
pub fn notice(c: &rusqlite::Connection, r: &records::Record, code: &str) -> anyhow::Result<Option<(bool, String)>> {
    Ok(Some(match code {
        "sent" => {
            let reply = r.data.get("reply").filter(|v| v.get("state").and_then(serde_json::Value::as_str) == Some("sent"));
            let url = reply.and_then(|v| v.get("url")).and_then(serde_json::Value::as_str).unwrap_or(r.str("url"));
            (true, format!("Sent: {url}. It's watched like the others; replies come back here as drafts."))
        }
        "asked" => (true, "Sent back to the agent with your note and your edits. It comes back here when the agent sends a new version.".into()),
        "dropped" => (true, "Dropped. Nothing was sent; the agent knows.".into()),
        "changed" => (false, "The draft changed since you opened it, so nothing was sent. This is the new version.".into()),
        "nonote" => (false, "Write what should change first. Nothing was sent.".into()),
        "failed" => {
            let last: Option<String> = c
                .query_row(
                    "SELECT summary FROM events WHERE record_id = ?1 AND kind = 'contribution.failed' ORDER BY seq DESC LIMIT 1",
                    [r.id],
                    |row| row.get(0),
                )
                .ok();
            (false, last.unwrap_or_else(|| "That didn't work; nothing was sent. See the log.".into()))
        }
        _ => return Ok(None),
    }))
}

/// The whole diff of a draft, as text.
pub async fn diff(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((slug, id)): Path<(String, String)>,
) -> AppResult<Response> {
    if matches!(actor, Actor::Anonymous) {
        return Ok(StatusCode::UNAUTHORIZED.into_response());
    }
    let r = find(&state, slug, id).await?;
    let text = outgoing::pending(&r).and_then(|p| outgoing::diff_text(&p)).ok_or(AppError::NotFound)?;
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (axum::http::header::CONTENT_SECURITY_POLICY, "default-src 'none'"),
        ],
        text,
    )
        .into_response())
}
