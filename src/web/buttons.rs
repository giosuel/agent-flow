//! The owner's buttons on PRs and releases: what the pages show, and pressing them.

use super::tone::tone;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::post;
use axum::{Extension, Form, Router};
use rusqlite::Connection;
use serde::Deserialize;

use super::{AppError, AppResult, AppState, ClientIp};
use crate::actions::{self, ActionRow};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::records::{self, Kind, Record};
use crate::util::ago;
use crate::{poller, projects, prs};

pub fn routes() -> Router<AppState> {
    Router::new().route("/actions/{id}/run", post(run_one))
}

/// A button as the page shows it.
pub struct ButtonView {
    pub id: i64,
    pub confirm: actions::Confirm,
    pub label: String,
    pub hash: String,
    pub command: String,
    pub workdir: String,
    pub added_by: String,
    pub enabled: bool,
    pub why: String,
    pub state: String,
    pub output: String,
    pub result: String,
}

pub fn view(c: &Connection, a: &ActionRow, now: i64) -> anyhow::Result<ButtonView> {
    let why = actions::enabled(c, a)?.err().unwrap_or_default();
    Ok(ButtonView {
        id: a.id,
        confirm: actions::confirm(c, a)?,
        label: a.label.clone(),
        hash: a.hash(),
        command: a.command.clone(),
        workdir: if a.workdir.is_empty() { "~".into() } else { a.workdir.clone() },
        added_by: a.added_by.clone(),
        enabled: why.is_empty(),
        why,
        state: a.state.clone(),
        output: a.output.clone(),
        result: match (a.state.as_str(), a.finished_at) {
            ("done", Some(t)) => format!("ran {}, exit 0", ago(t, now)),
            ("failed", Some(t)) => format!(
                "failed {}, exit {}",
                ago(t, now),
                a.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "none".into())
            ),
            ("running", _) => "running…".into(),
            _ => String::new(),
        },
    })
}

pub fn views(c: &Connection, record_id: i64, now: i64) -> anyhow::Result<Vec<ButtonView>> {
    actions::for_record(c, record_id)?.iter().map(|a| view(c, a, now)).collect()
}

/// A PR in the Pull requests tab: one read-only line in merge order (the owner, 2026-11-26, Q-37,
/// D-294: merged on GitHub, "like not even have the merge functionality inside agentflow").
pub struct PrLine {
    /// Its place in the merge order; drafts have none.
    pub position: usize,
    pub number: i64,
    pub title: String,
    /// The PR on GitHub.
    pub url: String,
    pub chip: String,
    pub chip_class: &'static str,
    /// The open PR it's based on, which merges first: its number and its page on GitHub.
    pub after: Option<(i64, String)>,
    /// A word on what happens about a problem: "the agent was told".
    pub note: String,
    pub draft: bool,
}

/// The project's open PRs in merge order, drafts last.
pub fn pr_lines(c: &Connection, project_id: i64) -> anyhow::Result<Vec<PrLine>> {
    let order = prs::merge_order(c, project_id)?;
    let url_of = |n: i64| order.iter().find(|o| o.record.number == n).map(|o| o.record.str("url").to_owned());
    let mut lines = Vec::new();
    for o in &order {
        let r = &o.record;
        let draft = r.data.get("draft").and_then(serde_json::Value::as_bool).unwrap_or(false);
        let (chip, chip_class, note) = if draft {
            ("draft".to_owned(), tone("draft"), "")
        } else {
            match prs::ready_in_order(o) {
                Ok(()) => ("ready".to_owned(), tone("ready"), ""),
                Err(_) if r.str("ci") == "failing" => ("CI failing".to_owned(), tone("failing"), "the agent was told"),
                Err(_) if r.str("mergeable") == "conflicting" => {
                    ("conflicts".to_owned(), tone("failing"), "the agent was told")
                }
                Err(_) if r.str("ci") == "pending" => ("CI running".to_owned(), tone("running"), ""),
                Err(why) => (why, tone("waiting"), ""),
            }
        };
        lines.push(PrLine {
            position: 0,
            number: r.number,
            title: r.title.clone(),
            url: r.str("url").to_owned(),
            chip,
            chip_class,
            after: o.after.map(|n| (n, url_of(n).unwrap_or_default())),
            note: note.to_owned(),
            draft,
        });
    }
    // Drafts fold away under the list; the others keep the merge order, numbered from 1.
    lines.sort_by_key(|l| l.draft);
    for (i, l) in lines.iter_mut().filter(|l| !l.draft).enumerate() {
        l.position = i + 1;
    }
    Ok(lines)
}

/// One of the repository's earlier tags beside the next release.
pub struct TagLine {
    pub name: String,
    pub title: String,
    pub when: String,
    /// Its page on GitHub.
    pub url: String,
}

/// Tags the Releases card lists, newest first.
pub const TAGS_SHOWN: i64 = 8;

/// The repository's newest tags for the Releases card, and how many it has in all.
pub fn tag_lines(
    c: &Connection,
    project_id: i64,
    repo: &str,
    tz: &crate::tz::Tz,
) -> anyhow::Result<(Vec<TagLine>, i64)> {
    let (tags, total) = prs::repo_tags(c, project_id, TAGS_SHOWN)?;
    let repo = repo.trim();
    Ok((
        tags.into_iter()
            .map(|t| TagLine {
                url: format!("https://github.com/{repo}/releases/tag/{}", t.name),
                when: if t.at > 0 { tz.day(t.at) } else { String::new() },
                name: t.name,
                title: t.title,
            })
            .collect(),
        total,
    ))
}

/// A release card.
pub struct ReleaseRow {
    pub tag: String,
    pub title: String,
    pub url: String,
    pub waits: String,
    /// The PRs it still waits for: "#58" and its page.
    pub open_prs: Vec<(String, String)>,
    pub buttons: Vec<ButtonView>,
}

pub fn release_rows(c: &Connection, project_id: i64, now: i64) -> anyhow::Result<Vec<ReleaseRow>> {
    let mut out = Vec::new();
    let list = records::query(
        c,
        &records::Query {
            project_id: Some(project_id),
            kinds: vec![Kind::Release],
            states: vec!["waiting".into(), "planned".into()],
            ..Default::default()
        },
    )?;
    for r in list {
        let waits: Vec<String> = r
            .data
            .get("waits_for")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        let open_prs: Vec<(String, String)> = waits
            .iter()
            .filter_map(|w| records::find_str(c, w, None).ok())
            .filter(|x| x.state != "merged")
            .map(|x| (format!("#{}", x.number), x.url()))
            .collect();
        out.push(ReleaseRow {
            tag: r.str("tag").to_owned(),
            title: r.title.clone(),
            url: r.url(),
            waits: match (waits.len(), open_prs.len()) {
                (0, _) => "Ready to tag: it waits for no PRs.".into(),
                (n, 0) => format!("Ready to tag: all {n} of its PRs are merged."),
                (n, k) => format!("Waiting for {k} of its {n} PRs:"),
            },
            open_prs,
            buttons: views(c, r.id, now)?,
        });
    }
    Ok(out)
}

#[derive(Deserialize)]
struct RunForm {
    hash: String,
    next: Option<String>,
}

fn safe_next(next: Option<String>, fallback: String) -> String {
    super::local_path(next.as_deref()).unwrap_or(fallback)
}

async fn run_one(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Path(id): Path<i64>,
    Form(form): Form<RunForm>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let (record, ok) = press(&state, &actor, &ip, id, &form.hash).await?;
    let next = safe_next(form.next, record.url());
    let sep = if next.contains('?') { '&' } else { '?' };
    Ok(Redirect::to(&format!("{next}{sep}ran={id}&ok={}", u8::from(ok))).into_response())
}

/// Claims, runs and records one button; refreshes a merged PR right away.
async fn press(state: &AppState, actor: &Actor, ip: &ClientIp, id: i64, hash: &str) -> AppResult<(Record, bool)> {
    let hash = hash.to_owned();
    let claimed = state
        .db
        .call(move |c| actions::claim(c, id, &hash))
        .await
        .map_err(|e| AppError::BadRequest(format!("{e:#}")))?;
    // The run lives in its own task: if the phone drops the connection, the merge still finishes
    // and is recorded. The request waits for it as long as it stays open.
    let (state2, actor2, ip2) = (state.clone(), actor.clone(), ip.clone());
    let run = tokio::spawn(async move {
        let started = std::time::Instant::now();
        let result = actions::execute(&claimed).await;
        let ok = result.ok;
        let a = claimed.clone();
        let summary = format!(
            "pressed \"{}\": {}",
            a.label,
            if ok { "done".to_owned() } else { format!("failed, exit {:?}", result.exit_code) }
        );
        let record = state2
            .db
            .call(move |c| {
                actions::finish(c, &a, &result)?;
                records::get(c, a.record_id)
            })
            .await?;
        let e = Entry::new(&actor2, Via::Button, Action::Run, summary)
            .project(record.project.clone())
            .target(record.ref_id.clone())
            .duration_ms(started.elapsed().as_millis() as i64)
            .ip(Some(ip2.0.clone()))
            .severity(if ok { "info" } else { "warn" });
        activity::write(&state2.db, e).await;
        if ok && record.kind == Kind::Pr {
            refresh_pr(&state2, &record).await;
        }
        anyhow::Ok((record, ok))
    });
    let (record, ok) = run.await.map_err(|e| AppError::Internal(e.into()))??;
    Ok((record, ok))
}

async fn refresh_pr(state: &AppState, record: &Record) {
    let slug = record.project.clone();
    let Ok(project) = state.db.call(move |c| projects::require(c, &slug)).await else { return };
    let repo = if record.str("repo").is_empty() { project.repo.clone() } else { record.str("repo").to_owned() };
    match state.github.pr(&repo, record.number).await {
        Ok(info) => {
            if let Err(e) = poller::apply(state, &project, &repo, info).await {
                tracing::warn!("refreshing {} after a press: {e:#}", record.ref_id);
            }
        }
        Err(e) => tracing::warn!("refreshing {} after a press: {e:#}", record.ref_id),
    }
}
