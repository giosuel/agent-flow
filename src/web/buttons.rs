//! The owner's buttons on PRs and releases: what the pages show, and pressing them.

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
    Router::new().route("/actions/{id}/run", post(run_one)).route("/p/{slug}/merge-up-to/{number}", post(merge_up_to))
}

/// A button as the page shows it.
pub struct ButtonView {
    pub id: i64,
    pub confirm: String,
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
        confirm: actions::confirm_text(a),
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

/// A PR in the merge-order card.
pub struct PrRow {
    pub position: usize,
    pub number: i64,
    pub title: String,
    pub url: String,
    pub record_url: String,
    pub chip: String,
    pub chip_class: &'static str,
    /// "based on main", or "based on #48" for a stacked PR.
    pub base: String,
    pub buttons: Vec<ButtonView>,
    /// "Merge #48–#50": every PR from the bottom of the stack up to this one, in order: the
    /// label, the buttons' "id:hash" list, and the dialog listing every command.
    pub up_to: Option<(String, String, String)>,
}

pub fn pr_rows(c: &Connection, project_id: i64, now: i64) -> anyhow::Result<Vec<PrRow>> {
    let order = prs::merge_order(c, project_id)?;
    let mut rows: Vec<PrRow> = Vec::new();
    // The run of mergeable PRs, from a stack's bottom, that "Merge up to here" would merge.
    let mut chain: Vec<(i64, i64, String, String)> = Vec::new();
    for o in &order {
        let r = &o.record;
        let buttons = views(c, r.id, now)?;
        let ready = prs::merge_ready(r);
        let (chip, chip_class) = match (&ready, o.after) {
            (Ok(()), None) => ("ready".to_owned(), "ok"),
            (Ok(()), Some(_)) => ("ready".to_owned(), "accent"),
            // Not marked ready is the normal state of work in progress, not a problem.
            (Err(why), _) if why.contains("marked it not ready") => ("not ready yet".to_owned(), "quiet"),
            (Err(why), _) => {
                (why.clone(), if r.str("ci") == "failing" || r.str("mergeable") == "conflicting" { "bad" } else { "" })
            }
        };
        let base = match o.after {
            Some(n) => format!("based on #{n}, merges after it"),
            None if r.str("base").is_empty() => String::new(),
            None => format!("based on {}", r.str("base")),
        };
        let merge_button = actions::for_record(c, r.id)?.into_iter().find(|a| a.is_merge() && a.state == "waiting");
        // A chain continues while each PR is based on the previous one, ready, and has a merge button.
        let continues = o.after.is_some_and(|after| chain.last().is_some_and(|(n, ..)| *n == after));
        if !continues {
            chain.clear();
        }
        let up_to = match (&ready, &merge_button) {
            (Ok(()), Some(a)) if o.after.is_none() || continues => {
                chain.push((r.number, a.id, a.hash(), a.command.clone()));
                (chain.len() > 1).then(|| {
                    let label = format!("Merge #{}–#{}", chain[0].0, r.number);
                    let ids = chain.iter().map(|(_, id, h, _)| format!("{id}:{h}")).collect::<Vec<_>>().join(",");
                    let confirm = format!(
                        "{label}? These run in order and stop at the first failure:\n\n{}",
                        chain.iter().map(|(n, _, _, cmd)| format!("#{n}: {cmd}")).collect::<Vec<_>>().join("\n")
                    );
                    (label, ids, confirm)
                })
            }
            _ => {
                chain.clear();
                None
            }
        };
        rows.push(PrRow {
            position: o.position,
            number: r.number,
            title: r.title.clone(),
            url: r.str("url").to_owned(),
            record_url: r.url(),
            chip,
            chip_class,
            base,
            buttons,
            up_to,
        });
    }
    Ok(rows)
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

#[derive(Deserialize)]
struct UpToForm {
    /// "id:hash,id:hash": the merge buttons the page showed, bottom of the stack first.
    actions: String,
}

/// "Merge #48–#50": presses each PR's merge button in order, stopping at the first failure.
async fn merge_up_to(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Path((slug, number)): Path<(String, i64)>,
    Form(form): Form<UpToForm>,
) -> AppResult<Response> {
    actor.require_owner()?;
    if crate::projects::check_slug(&slug).is_err() {
        return Err(AppError::NotFound);
    }
    let mut steps = Vec::new();
    for part in form.actions.split(',').filter(|p| !p.is_empty()) {
        let (id, hash) = part.split_once(':').ok_or_else(|| AppError::BadRequest("malformed button list".into()))?;
        steps.push((
            id.parse::<i64>().map_err(|_| AppError::BadRequest("malformed button list".into()))?,
            hash.to_owned(),
        ));
    }
    if steps.is_empty() {
        return Err(AppError::BadRequest("nothing to merge".into()));
    }
    let mut merged = 0;
    for (id, hash) in steps {
        let (_, ok) = press(&state, &actor, &ip, id, &hash).await?;
        if !ok {
            break;
        }
        merged += 1;
    }
    Ok(Redirect::to(&format!("/p/{slug}?tab=prs&merged={merged}&upto={number}")).into_response())
}
