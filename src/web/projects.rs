//! The Projects list, a project's page (overview, tasks, decisions, questions) and a record's page.

use std::collections::HashMap;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Form, Router};
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::Value;

use super::icons::icon;
use super::pages::{Nav, nav, render};
use super::{AppError, AppResult, AppState, ClientIp};
use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::markdown;
use crate::projects::{self, Milestone, Project};
use crate::records::{self, Kind, Record};
use crate::tz::Tz;
use crate::util::{ago, now_ms};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/projects", get(list))
        .route("/p/{slug}", get(project))
        .route("/p/{slug}/{id}", get(record))
        .route("/p/{slug}/{id}/answer", post(answer))
        .route("/p/{slug}/status", post(set_status))
        .route("/p/{slug}/settings", post(save_settings))
}

fn tested_notice(n: usize) -> String {
    if n == 0 {
        "Nothing was ticked, so nothing was sent.".into()
    } else {
        format!("Sent: {n} tested, works. The agent hears which.")
    }
}

/// The Settings tab's form: every detail an agent can set with update_project, the owner here (the
/// owner, 2026-11-11: "a settings tab for projects where the user can archive it or rename it in
/// agent flow, or add / edit other metadata"). Lists are one per line; the stack is comma-separated.
#[derive(Deserialize)]
struct SettingsForm {
    name: String,
    description: String,
    repo: String,
    checkout: String,
    machine: String,
    model: String,
    effort: String,
    ports: String,
    build_product: String,
    #[serde(default)]
    merge_method: String,
    /// The build server for `agentflow remote` ("" for none).
    #[serde(default)]
    build_server: String,
}

async fn save_settings(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Path(slug): Path<String>,
    Form(f): Form<SettingsForm>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let back = |code: &str| -> AppResult<Response> {
        Ok(Redirect::to(&format!("/p/{slug}?tab=settings&settings={code}")).into_response())
    };
    if f.name.trim().is_empty() {
        return back("name");
    }
    if !f.repo.trim().is_empty() && crate::github::check_repo(f.repo.trim()).is_err() {
        return back("repo");
    }
    let lines = |s: &str| s.lines().map(str::to_owned).collect::<Vec<_>>();
    let patch = projects::ProjectPatch {
        name: Some(f.name.clone()),
        description: Some(f.description.clone()),
        repo: Some(f.repo.clone()),
        checkout: Some(f.checkout.clone()),
        machine: Some(f.machine.clone()),
        model: Some(f.model.clone()),
        effort: Some(f.effort.clone()),
        ports: Some(lines(&f.ports)),
        build_product: Some(f.build_product.clone()),
        merge_method: if f.merge_method.is_empty() { None } else { Some(f.merge_method.clone()) },
        ..Default::default()
    };
    let s = slug.clone();
    let server = f.build_server.trim().to_owned();
    let result = state
        .db
        .call(move |c| {
            let p = projects::require(c, &s)?;
            let mut changed = projects::update(c, p.id, &patch, "owner")?;
            if crate::remote::project_server(c, p.id)?.unwrap_or_default() != server {
                crate::remote::set_project_server(c, p.id, &server)?;
                changed.push("build server");
            }
            Ok(changed)
        })
        .await;
    match result {
        Ok(changed) => {
            if !changed.is_empty() {
                let what = format!("project settings changed: {}", changed.join(", "));
                let e = Entry::new(&actor, Via::Page, Action::Write, what).project(slug.clone()).ip(Some(ip.0));
                activity::write(&state.db, e).await;
            }
            back("saved")
        }
        Err(e) if format!("{e:#}").contains("build") || format!("{e:#}").contains("product") => back("product"),
        Err(e) => {
            tracing::warn!("saving {slug}'s settings: {e:#}");
            back("failed")
        }
    }
}

#[derive(Deserialize)]
struct StatusForm {
    status: String,
}

/// Archive a project, or bring it back (the owner, 2026-11-11: "how do I delete (or rather
/// archive) projects?"). Nothing is deleted: an archived project leaves the Dashboard, the
/// waiting count and the project list, and its page and records stay.
async fn set_status(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Path(slug): Path<String>,
    Form(f): Form<StatusForm>,
) -> AppResult<Response> {
    actor.require_owner()?;
    if !matches!(f.status.as_str(), "active" | "archived") {
        return Err(AppError::BadRequest("status is active or archived".into()));
    }
    let (s, st) = (slug.clone(), f.status.clone());
    let name = state
        .db
        .call(move |c| {
            let p = projects::require(c, &s)?;
            projects::update(c, p.id, &projects::ProjectPatch { status: Some(st), ..Default::default() }, "owner")?;
            Ok(p.name)
        })
        .await?;
    let what = if f.status == "archived" { "archived" } else { "restored" };
    let e = Entry::new(&actor, Via::Page, Action::Write, format!("project {name} {what}"))
        .project(slug.clone())
        .ip(Some(ip.0));
    activity::write(&state.db, e).await;
    Ok(Redirect::to(&format!("/p/{slug}?tab=settings")).into_response())
}

#[derive(Deserialize, Default)]
struct ListQuery {
    archived: Option<u8>,
}

// ---------------------------------------------------------------------------------------------
// Shared view pieces

/// One line in a list of records.
pub struct RecordRow {
    pub id: String,
    pub short: String,
    pub url: String,
    pub title: String,
    pub state: String,
    pub sub: String,
}

impl RecordRow {
    pub fn new(r: &Record, sub: String) -> Self {
        Self {
            id: r.ref_id.clone(),
            short: r.short_id(),
            url: r.url(),
            title: r.title.clone(),
            state: r.state.clone(),
            sub,
        }
    }
}

/// "M8 · Text and documents": the active milestone, or the next planned one.
fn current_milestone(ms: &[Milestone]) -> Option<&Milestone> {
    let top = || ms.iter().filter(|m| m.parent_id.is_none());
    top().find(|m| m.state == "active").or_else(|| top().find(|m| m.state == "planned"))
}

/// What waits on the owner per project, as the nav's count has it: open questions and builds to try.
fn waiting_counts(c: &Connection) -> anyhow::Result<HashMap<i64, (i64, i64)>> {
    let mut stmt = c.prepare(
        "SELECT project_id,
                sum(kind = 'question' AND state = 'open'),
                sum(kind = 'build' AND state = 'published' AND coalesce(json_extract(data, '$.ready_for_feedback'), 0))
         FROM records WHERE kind IN ('question', 'build') GROUP BY project_id",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

// ---------------------------------------------------------------------------------------------
// Projects list

/// One project on the Projects page: a full-width row (the owner, 2026-11-11, chose this over
/// cards: "lets go with projects: A").
struct ProjectCard {
    slug: String,
    name: String,
    description: String,
    online: bool,
    agent_state: &'static str,
    /// Its agent has never connected: listed small, under Not started.
    not_started: bool,
    facts: String,
    /// Top-level milestones around the current one: (name, "d" done, "n" now, "" planned).
    ms_chips: Vec<(String, &'static str)>,
    milestone: Option<(String, String)>,
    /// "step S2 · 2 of 8 tasks done", and the share done.
    progress: String,
    percent: u32,
    /// What waits on the owner: (chip class, text).
    waits: Vec<(&'static str, String)>,
    /// The project's latest event: "asked Q-1 · 13 h ago".
    last: String,
    /// New tokens per day for 7 days: (bar height %, tip), and the week's total.
    tokens: Vec<(u32, String)>,
    tokens_week: String,
    status: String,
}

#[derive(Template)]
#[template(path = "projects.html")]
struct ProjectsPage {
    nav: Nav,
    cards: Vec<ProjectCard>,
    /// Showing the archived projects instead of the others.
    archived: bool,
    archived_count: usize,
    /// Some project's agent never connected (they're listed small, under Not started).
    any_not_started: bool,
}

async fn list(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<ListQuery>,
    tz: Tz,
) -> AppResult<Response> {
    let now = now_ms();
    let archived = q.archived == Some(1);
    let week_days = tz.days(now, 7);
    let day_names: Vec<String> =
        week_days.iter().take(7).map(|&t| tz.full(t).get(..10).unwrap_or("").to_owned()).collect();
    let cards: Vec<ProjectCard> = state
        .db
        .call(move |c| {
            let counts = waiting_counts(c)?;
            let mut cards = Vec::new();
            for p in projects::list(c)? {
                let ms = projects::milestones(c, p.id)?;
                let online = p.agent.online(now);
                let facts: Vec<String> = Vec::new();
                // The milestones around the current one, and how far it is.
                let top: Vec<&Milestone> = ms.iter().filter(|m| m.parent_id.is_none()).collect();
                let cur = current_milestone(&ms);
                let cur_i = cur.and_then(|m| top.iter().position(|t| t.id == m.id)).unwrap_or(top.len());
                let from = cur_i.saturating_sub(3);
                let ms_chips = top
                    .iter()
                    .enumerate()
                    .skip(from)
                    .take(6)
                    .map(|(i, m)| {
                        (
                            m.name.clone(),
                            if i < cur_i || m.state == "closed" {
                                "d"
                            } else if i == cur_i {
                                "n"
                            } else {
                                ""
                            },
                        )
                    })
                    .collect();
                let (progress, percent) = match cur {
                    Some(m) => {
                        let ids: Vec<i64> = std::iter::once(m.id)
                            .chain(ms.iter().filter(|s| s.parent_id == Some(m.id)).map(|s| s.id))
                            .collect();
                        let tasks = records::query(
                            c,
                            &records::Query {
                                project_id: Some(p.id),
                                kinds: vec![Kind::Task],
                                milestone_ids: ids,
                                limit: 0,
                                ..Default::default()
                            },
                        )?;
                        let done = tasks.iter().filter(|t| t.state == "done").count();
                        let step = ms.iter().find(|s| s.parent_id == Some(m.id) && s.state == "active");
                        let line = match (step, tasks.len()) {
                            (_, 0) => "no tasks yet".to_owned(),
                            (Some(s), n) => format!("step {} · {done} of {n} tasks done", s.name),
                            (None, n) => format!("{done} of {n} tasks done"),
                        };
                        (line, if tasks.is_empty() { 0 } else { (done * 100 / tasks.len()) as u32 })
                    }
                    None => (String::new(), 0),
                };
                let (questions, _) = counts.get(&p.id).copied().unwrap_or((0, 0));
                let mut waits = Vec::new();
                if questions > 0 {
                    waits.push(("bad", plural(questions, "question", "questions")));
                }
                let merges = crate::prs::merge_order(c, p.id)?
                    .iter()
                    .filter(|o| o.after.is_none() && crate::prs::ready_in_order(o).is_ok())
                    .count() as i64;
                if merges > 0 {
                    waits.push(("accent", format!("{merges} to merge")));
                }
                let to_test = crate::totest::open_count(c, p.id)?;
                if to_test > 0 {
                    waits.push(("warn", format!("{to_test} to test")));
                }
                let last = records::recent_events(c, Some(p.id), 1)?
                    .first()
                    .map(|e| format!("{} · {}", e.summary, crate::util::ago(e.at, now)))
                    .unwrap_or_default();
                let week = crate::usage::project_days(c, p.id, &week_days)?;
                let max = week.iter().copied().max().unwrap_or(0).max(1);
                let tokens = week
                    .iter()
                    .enumerate()
                    .map(|(i, &n)| {
                        (
                            ((n * 100) / max) as u32,
                            format!("{}: {}", day_names.get(i).cloned().unwrap_or_default(), crate::usage::short(n)),
                        )
                    })
                    .collect();
                let tokens_week = crate::usage::short(week.iter().sum());
                cards.push(ProjectCard {
                    online,
                    agent_state: if online {
                        "online"
                    } else if p.agent.seen_at.is_some() {
                        "idle"
                    } else {
                        "no agent yet"
                    },
                    not_started: p.agent.seen_at.is_none(),
                    ms_chips,
                    milestone: cur.map(|m| (m.name.clone(), m.title.clone())),
                    progress,
                    percent,
                    waits,
                    last,
                    tokens,
                    tokens_week,
                    facts: facts.join(" · "),
                    status: p.status.clone(),
                    slug: p.slug,
                    name: p.name,
                    description: p.description,
                });
            }
            Ok(cards)
        })
        .await?;
    let archived_count = cards.iter().filter(|c| c.status == "archived").count();
    let cards: Vec<ProjectCard> = cards.into_iter().filter(|c| (c.status == "archived") == archived).collect();
    let any_not_started = cards.iter().any(|c| c.not_started);
    let page_nav = nav(&state, &actor, "/projects").await;
    render(ProjectsPage {
        nav: if archived { page_nav.back_to("/projects", "Projects") } else { page_nav },
        cards,
        archived,
        archived_count,
        any_not_started,
    })
}

fn model_line(p: &Project) -> String {
    match (p.model.as_str(), p.effort.as_str()) {
        ("", "") => String::new(),
        (m, "") => m.into(),
        ("", e) => format!("effort {e}"),
        (m, e) => format!("{m} · effort {e}"),
    }
}

// ---------------------------------------------------------------------------------------------
// A project's page

struct MetaRow {
    icon: &'static str,
    label: &'static str,
    value: String,
    list: Vec<String>,
}

struct MsView {
    name: String,
    title: String,
    class: &'static str,
    note: String,
    /// Its tasks are on the board below (the owner, 2026-11-11: "make the milestones into
    /// buttons, and the kanban board switches based on the selected milestone").
    selected: bool,
}

struct StepView {
    name: String,
    title: String,
    class: &'static str,
}

struct TaskCard {
    short: String,
    url: String,
    title: String,
    step: String,
    sub: String,
    blocked_by: Vec<(String, String)>,
}

struct Column {
    title: &'static str,
    tasks: Vec<TaskCard>,
}

struct Board {
    title: String,
    summary: String,
    steps: Vec<StepView>,
    columns: Vec<Column>,
}

/// A milestone on the Tasks tab: its own tasks, then its steps (the owner, 2026-11-12).
struct TaskGroup {
    name: String,
    title: String,
    state: String,
    sub: String,
    rows: Vec<TaskLine>,
    steps: Vec<StepBox>,
}

/// A step: concluded (one line and its summary), under way (its tasks in order) or planned.
struct StepBox {
    name: String,
    title: String,
    review: bool,
    /// concluded, active, blocked (every open task is) or planned.
    state: &'static str,
    sub: String,
    url: String,
    rows: Vec<TaskLine>,
}

/// One task in the agent's order, with what blocks it.
struct TaskLine {
    n: usize,
    short: String,
    url: String,
    title: String,
    state: String,
    est: String,
    next: bool,
    /// Open questions it waits for: (Q-1, url).
    blocked_by: Vec<(String, String)>,
    blocked_note: String,
    /// "added · your feedback": a task that turned up while its step was under way.
    added: String,
    skip_reason: String,
    /// Time spent (its share), when it has any.
    spent: String,
}

/// How a task's source reads on the page.
pub(super) fn source_label(source: &str) -> &'static str {
    match source {
        "owner_feedback" => "your feedback",
        "found" => "found while building",
        "review" => "review",
        "bug" => "bug",
        _ => "planned",
    }
}

/// One PR or issue on someone else's repository, or a draft of one, on the project's overview.
pub struct ExtRow {
    icon: &'static str,
    title: String,
    sub: String,
    url: String,
    /// A chip: (class, text), e.g. ("warn", "1 new review").
    chip: Option<(&'static str, String)>,
    /// A button for a draft waiting for the owner.
    review: bool,
}

/// The overview's "External PRs / issues": drafts first, then those still open, then the rest
/// (merged, closed, dropped) folded away. The owner, 2026-11-11: "below project agent and above
/// open questions".
fn external_rows(c: &Connection, all: &[Record], now: i64) -> anyhow::Result<(Vec<ExtRow>, Vec<ExtRow>)> {
    let week_ago = now - 7 * 24 * 3600 * 1000;
    let mut news = c.prepare(
        "SELECT summary FROM events WHERE record_id = ?1 AND kind = 'contribution.updated' AND at > ?2
         ORDER BY seq DESC LIMIT 1",
    )?;
    let mut open: Vec<(u8, i64, ExtRow)> = Vec::new();
    let mut done = Vec::new();
    for r in all.iter().filter(|r| r.kind == Kind::Contribution) {
        let number = r.data.get("number").and_then(Value::as_i64);
        let short = |r: &Record| match number {
            Some(n) => format!("{} #{n}", r.str("repo")),
            None => r.str("repo").to_owned(),
        };
        // The PR's or issue's own title, without "repo#n: ".
        let own_title =
            r.title.split_once(": ").filter(|_| number.is_some()).map_or(r.title.as_str(), |(_, t)| t).to_owned();
        if let Some(p) = crate::outgoing::pending(r) {
            let what = match p.kind {
                crate::outgoing::DraftType::Pr => format!("New PR to {}", r.str("repo")),
                crate::outgoing::DraftType::Issue => format!("New issue for {}", r.str("repo")),
                crate::outgoing::DraftType::Comment => format!("Comment on {}", short(r)),
                crate::outgoing::DraftType::Commits => format!("New commits for {}", short(r)),
            };
            let (rank, chip, review) = match p.state {
                "shown" => (0, None, true),
                "sending" => (0, Some(("accent", "sending".to_owned())), false),
                _ => (1, Some(("quiet", "changes asked".to_owned())), false),
            };
            let sub = if p.kind.is_new() { format!("Draft · {}", p.title) } else { format!("Draft · {own_title}") };
            open.push((rank, -r.updated_at, ExtRow { icon: "draft", title: what, sub, url: r.url(), chip, review }));
            continue;
        }
        let icon = if r.str("type") == "issue" { "issue" } else { "pr" };
        let row = |chip| ExtRow { icon, title: short(r), sub: own_title.clone(), url: r.url(), chip, review: false };
        match r.state.as_str() {
            "sent" => {
                let latest: Option<String> =
                    news.query_row(rusqlite::params![r.id, week_ago], |row| row.get(0)).optional()?;
                // "repo#4: Title: 1 new review (url)" → "1 new review"
                let chip = latest
                    .as_deref()
                    .and_then(|s| s.rsplit_once(" (").map(|(a, _)| a))
                    .and_then(|s| s.rsplit_once(": ").map(|(_, n)| n.to_owned()))
                    .map(|n| ("warn", n));
                open.push((2, -r.updated_at, row(chip)));
            }
            "merged" => done.push(row(Some(("ok", "merged".into())))),
            "closed" => done.push(row(Some(("quiet", "closed".into())))),
            "dropped" => done.push(ExtRow {
                icon: "draft",
                title: format!("Draft for {}", r.str("repo")),
                sub: r.title.clone(),
                url: r.url(),
                chip: Some(("quiet", "dropped".into())),
                review: false,
            }),
            _ => {}
        }
    }
    open.sort_by_key(|(rank, t, _)| (*rank, *t));
    Ok((open.into_iter().map(|(_, _, r)| r).collect(), done))
}

struct AgentCard {
    session: String,
    machine: String,
    model: String,
    working_on: String,
    seen: String,
    online: bool,
    /// New tokens over the last 7 days, and a bar per day (height in %, its title), oldest first.
    tokens_week: String,
    strip: Vec<(u32, String)>,
}

pub struct EditView {
    /// The build server its `agentflow remote` uses ("" for none), and the ones there are.
    build_server: String,
    servers: Vec<String>,
    name: String,
    description: String,
    repo: String,
    checkout: String,
    machine: String,
    model: String,
    effort: String,
    ports: String,
    build_product: String,
    merge_method: String,
}

pub struct ShotCard {
    url: String,
    image: String,
    title: String,
    category: String,
    when: String,
    /// A mockup's state, and whether it links to its design.
    mstate: String,
    design: bool,
}

pub struct ShotView {
    image: String,
    category: String,
    line: String,
    mstate: String,
    design_url: String,
    /// Where the owner's state buttons post.
    state_action: String,
}

pub struct Connect {
    mcp_add: String,
    start: String,
    token_file: String,
    has_file: bool,
}

#[derive(Template)]
#[template(path = "project.html")]
struct ProjectPage {
    /// The To test checklist (its tab), and how many are open (the tab's count).
    to_test: Option<super::totest::View>,
    to_test_open: i64,
    /// The Notes tab: the project document, and the notes.
    doc: Option<super::projectdoc::DocView>,
    notes: Option<super::notes::NotesView>,
    /// External PRs and issues: open ones and drafts, and the finished ones.
    external: Vec<ExtRow>,
    external_done: Vec<ExtRow>,
    /// The agent's Claude Code session on claude.ai, from its transcripts (remote control).
    session_url: Option<String>,
    nav: Nav,
    /// How to connect its agent, again (the owner, 2026-11-11: "make it so I can still get the
    /// mcp / setup commands for existing projects").
    connect: Option<Connect>,
    slug: String,
    name: String,
    repo: String,
    status: String,
    headline: String,
    tab: String,
    about: Vec<MetaRow>,
    milestones: Vec<MsView>,
    board: Option<Board>,
    agent: AgentCard,
    open_questions: Vec<RecordRow>,
    decisions: Vec<RecordRow>,
    /// Put away in a cleanup: folded away on the Decisions tab.
    decisions_archived: Vec<RecordRow>,
    groups: Vec<TaskGroup>,
    questions: Vec<RecordRow>,
    /// The Builds tab: every build, newest first.
    builds: Vec<RecordRow>,
    /// The Settings tab's form, filled with the project's details.
    edit: Option<EditView>,
    /// The Screenshots tab: the shown ones (newest first), all of them, and the category filter.
    shots: Vec<ShotCard>,
    shots_all: usize,
    shot_cats: Vec<(String, String, usize, bool)>,
    prs: Vec<super::buttons::PrRow>,
    prs_total: usize,
    has_stack_button: bool,
    tabs: &'static [(&'static str, &'static str)],
    releases: Vec<super::buttons::ReleaseRow>,
    notice: Option<String>,
    notice_bad: bool,
    /// Where a pressed button returns to.
    next: String,
}

#[derive(Deserialize)]
struct TabQuery {
    tab: Option<String>,
    /// After a button: its id and whether it worked.
    ran: Option<i64>,
    ok: Option<u8>,
    /// After "Merge up to here": how many merged, and up to which PR.
    merged: Option<usize>,
    upto: Option<i64>,
    /// The Screenshots tab's category filter.
    cat: Option<String>,
    /// The Settings tab's outcome: saved, or why not.
    settings: Option<String>,
    /// After Send on the To test list: how many were marked tested.
    tested: Option<usize>,
    /// The milestone whose board the overview shows (its name); the current one if left out.
    ms: Option<String>,
    /// The Notes tab: a section of the project document to edit ("2", or "all"), its history, an
    /// older revision, and how a save went.
    edit: Option<String>,
    history: Option<u8>,
    rev: Option<i64>,
    doc: Option<String>,
    /// The Notes tab's category filter.
    ncat: Option<String>,
}

/// The line shown after pressing a button, from the redirect's query.
fn press_notice(q: &TabQuery) -> (Option<String>, bool) {
    if let Some(n) = q.tested {
        return (Some(tested_notice(n)), false);
    }
    // The Settings tab's outcome: only fixed messages come from the address.
    match q.settings.as_deref() {
        Some("saved") => {
            return (Some("Saved. The project's agent sees the change the next time it checks.".into()), false);
        }
        Some("name") => return (Some("The name can't be empty. Nothing was saved.".into()), true),
        Some("repo") => {
            return (Some("The repository is owner/name, e.g. giosuel/vinculum. Nothing was saved.".into()), true);
        }
        Some("product") => {
            return (
                Some("That build product isn't free or isn't a valid name (lowercase letters, digits and dashes). Nothing was saved.".into()),
                true,
            );
        }
        Some(_) => return (Some("That didn't work; nothing was saved. See the log.".into()), true),
        None => {}
    }
    match (q.ran, q.ok, q.merged, q.upto) {
        (Some(_), Some(1), _, _) => (Some("Done. The agent has been told.".into()), false),
        (Some(_), Some(_), _, _) => {
            (Some("The command failed; its output is under the button. The agent has been told.".into()), true)
        }
        (_, _, Some(n), Some(to)) => {
            (Some(format!("{n} PR{} merged, up to #{to}.", if n == 1 { "" } else { "s" })), false)
        }
        _ => (None, false),
    }
}

/// The project page's tabs, in order; questions come right after the overview.
const TABS: &[(&str, &str)] = &[
    ("overview", "Overview"),
    ("questions", "Questions"),
    ("tasks", "Tasks"),
    ("prs", "Pull requests"),
    ("builds", "Builds"),
    ("decisions", "Decisions"),
    ("to-test", "To test"),
    ("screenshots", "Screenshots"),
    ("notes", "Notes"),
    ("settings", "Settings"),
];

async fn project(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(slug): Path<String>,
    Query(q): Query<TabQuery>,
    tz: Tz,
) -> AppResult<Response> {
    let (notice, notice_bad) = press_notice(&q);
    let tz_doc = tz.clone();
    let tab = q.tab.filter(|t| TABS.iter().any(|(k, _)| k == t)).unwrap_or_else(|| "overview".into());
    let now = now_ms();
    let chosen = q.ms.clone();
    let t = tab.clone();
    let page = state
        .db
        .call(move |c| {
            let Some(p) = projects::get(c, &slug)? else { return Ok(None) };
            let ms = projects::milestones(c, p.id)?;
            let all = records::query(c, &records::Query { project_id: Some(p.id), limit: 0, ..Default::default() })?;
            let checkout = p.checkout.trim().to_owned();
            Ok(Some((build_project_page(c, p, ms, all, &t, chosen.as_deref(), now, &tz)?, checkout)))
        })
        .await?
        .ok_or(AppError::NotFound)?;
    let (mut page, checkout) = page;
    if tab == "screenshots" {
        let mut cats: Vec<(String, usize)> = Vec::new();
        for s in &page.shots {
            match cats.iter_mut().find(|c| c.0 == s.category) {
                Some(c) => c.1 += 1,
                None => cats.push((s.category.clone(), 1)),
            }
        }
        cats.sort();
        let pick = q.cat.clone().filter(|c| cats.iter().any(|x| &x.0 == c));
        let base = format!("/p/{}?tab=screenshots", page.slug);
        let mut pills = vec![("All".to_owned(), base.clone(), page.shots.len(), pick.is_none())];
        for (c, n) in cats {
            let href = format!("{base}&cat={}", super::log::enc(&c));
            let on = pick.as_deref() == Some(c.as_str());
            pills.push((c, href, n, on));
        }
        if let Some(c) = &pick {
            page.shots.retain(|s| &s.category == c);
        }
        page.shot_cats = pills;
    }
    if tab == "overview" && actor.is_owner() && !checkout.is_empty() {
        let folder = crate::util::expand_home(&checkout);
        let token_file = crate::tokens::token_path(&state.config.token_dir(), Some(&page.slug), &page.name);
        let (mcp_add, start) = crate::onboard::commands(&folder, &token_file, &state.config.public_url);
        let has_file: bool =
            crate::hands::run_async(crate::hands::Job::TokenExists { path: token_file.clone() }).await.unwrap_or(true);
        page.connect = Some(Connect { mcp_add, start, token_file: token_file.display().to_string(), has_file });
    }
    if tab == "notes" {
        let (slug, edit, rev, outcome) = (page.slug.clone(), q.edit.clone(), q.rev, q.doc.clone());
        let history = q.history == Some(1);
        let owner = actor.is_owner();
        let tz2 = tz_doc;
        page.doc = Some(
            state
                .db
                .call(move |c| {
                    let p = projects::require(c, &slug)?;
                    let ask = super::projectdoc::Ask {
                        edit: edit.as_deref().filter(|_| owner),
                        history,
                        rev,
                        outcome: outcome.as_deref(),
                    };
                    super::projectdoc::view(c, p.id, &ask, &tz2, now)
                })
                .await?,
        );
        let (slug, pick) = (page.slug.clone(), q.ncat.clone());
        page.notes = Some(
            state
                .db
                .call(move |c| {
                    let p = projects::require(c, &slug)?;
                    super::notes::view(c, &slug, p.id, pick.as_deref(), now)
                })
                .await?,
        );
    }
    page.prs_total = page.prs.len();
    page.has_stack_button = page.prs.iter().any(|p| p.up_to.is_some());
    if tab == "overview" {
        // The overview shows the start of the order; the Pull requests tab has all of them.
        page.prs.truncate(8);
    }
    let next = if tab == "overview" { format!("/p/{}", page.slug) } else { format!("/p/{}?tab={tab}", page.slug) };
    let page = ProjectPage {
        nav: nav(&state, &actor, "/projects").await.back_to("/projects", "Projects"),
        tab,
        notice,
        notice_bad,
        next,
        ..page
    };
    render(page)
}

#[allow(clippy::too_many_arguments)]
fn build_project_page(
    c: &Connection,
    p: Project,
    ms: Vec<Milestone>,
    all: Vec<Record>,
    tab: &str,
    chosen: Option<&str>,
    now: i64,
    tz: &Tz,
) -> anyhow::Result<ProjectPage> {
    let week_days = tz.days(now, 7);
    let week = crate::usage::project_days(c, p.id, &week_days)?;
    let online = p.agent.online(now);
    let headline = {
        let mut parts = Vec::new();
        parts.push(match (p.agent.session.as_str(), p.agent.seen_at) {
            ("", None) => "no agent has connected yet".into(),
            (s, Some(t)) => format!(
                "agent: {} session, {}",
                if s.is_empty() { "its" } else { s },
                if online { "online now".into() } else { format!("last seen {}", ago(t, now)) }
            ),
            (s, None) => format!("agent: {s}"),
        });
        parts.join(" · ")
    };

    let mut about = Vec::new();
    let mut meta = |icon_name: &str, label: &'static str, value: String, list: Vec<String>| {
        if !value.is_empty() || !list.is_empty() {
            about.push(MetaRow { icon: icon(icon_name), label, value, list });
        }
    };
    meta("info", "About", p.description.clone(), vec![]);
    meta("model", "AI model", model_line(&p), vec![]);
    meta(
        "machine",
        "Machine",
        [p.machine.as_str(), p.checkout.as_str()]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" · "),
        vec![],
    );
    meta("port", "Ports", p.ports.join(", "), vec![]);

    // The board shows the chosen milestone (a top-level one), else the current one.
    let current = chosen
        .and_then(|name| ms.iter().find(|m| m.parent_id.is_none() && m.name.eq_ignore_ascii_case(name)))
        .or_else(|| current_milestone(&ms))
        .cloned();
    let now_ms_id = current_milestone(&ms).map(|m| m.id);
    let milestones = ms
        .iter()
        .filter(|m| m.parent_id.is_none())
        .map(|m| {
            let is_current = now_ms_id == Some(m.id);
            let step =
                ms.iter().filter(|s| s.parent_id == Some(m.id) && s.state == "active").map(|s| s.name.clone()).next();
            MsView {
                name: m.name.clone(),
                title: m.title.clone(),
                class: match (m.state.as_str(), is_current) {
                    ("closed", _) => "done",
                    (_, true) => "now",
                    _ => "up",
                },
                note: match m.state.as_str() {
                    "closed" if !m.tag.is_empty() => m.tag.clone(),
                    "closed" => "closed".into(),
                    "active" => match step {
                        Some(s) => format!("in progress · step {s}"),
                        None => "in progress".into(),
                    },
                    _ => "planned".into(),
                },
                selected: current.as_ref().is_some_and(|c| c.id == m.id),
            }
        })
        .collect();

    // Open questions that block a task, for "blocked by Q-31" on its card.
    let mut blockers: HashMap<i64, Vec<(String, String)>> = HashMap::new();
    {
        let mut stmt = c.prepare(
            "SELECT l.to_id, r.kind, r.number, p.slug FROM links l JOIN records r ON r.id = l.from_id JOIN projects p ON p.id = r.project_id
             JOIN records t ON t.id = l.to_id
             WHERE l.relation = 'blocks' AND r.state IN ('open', 'todo', 'doing', 'blocked') AND t.project_id = ?1",
        )?;
        let rows = stmt.query_map([p.id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?))
        })?;
        for row in rows {
            let (to, kind, n, slug) = row?;
            if let Ok(k) = Kind::parse(&kind) {
                let short = format!("{}-{n}", k.prefix());
                blockers.entry(to).or_default().push((format!("/p/{slug}/{short}"), short));
            }
        }
    }
    let step_name = |id: Option<i64>| -> String {
        id.and_then(|id| ms.iter().find(|m| m.id == id))
            .filter(|m| m.parent_id.is_some())
            .map(|m| m.name.clone())
            .unwrap_or_default()
    };
    let task_card = |r: &Record| TaskCard {
        short: r.short_id(),
        url: r.url(),
        title: r.title.clone(),
        step: step_name(r.milestone_id),
        sub: match r.state.as_str() {
            "doing" => r
                .data
                .get("started_at")
                .and_then(Value::as_i64)
                .map(|t| format!("started {}", ago(t, now)))
                .unwrap_or_default(),
            "parked" => match r.str("resurface_when") {
                "" => "parked".into(),
                w => format!("comes back {w}"),
            },
            _ => String::new(),
        },
        blocked_by: blockers.get(&r.id).cloned().unwrap_or_default(),
    };
    let tasks: Vec<&Record> = all.iter().filter(|r| r.kind == Kind::Task).collect();

    let board = current.as_ref().map(|cur| {
        let ids: Vec<i64> = projects::with_steps(&ms, cur.id);
        let mut mine: Vec<&Record> =
            tasks.iter().copied().filter(|t| t.milestone_id.is_some_and(|m| ids.contains(&m))).collect();
        mine.sort_by(|a, b| a.position.partial_cmp(&b.position).unwrap_or(std::cmp::Ordering::Equal));
        let col = |title: &'static str, states: &[&str]| Column {
            title,
            tasks: mine.iter().filter(|t| states.contains(&t.state.as_str())).map(|t| task_card(t)).collect(),
        };
        let count = |s: &str| mine.iter().filter(|t| t.state == s).count() as i64;
        let mut summary = vec![plural(mine.len() as i64, "task", "tasks")];
        if count("doing") > 0 {
            summary.push(format!("{} in progress", count("doing")));
        }
        if count("done") > 0 {
            summary.push(format!("{} done", count("done")));
        }
        if count("parked") > 0 {
            summary.push(format!("{} parked", count("parked")));
        }
        Board {
            title: format!("{} · {}", cur.name, cur.title),
            summary: summary.join(" · "),
            steps: ms
                .iter()
                .filter(|s| s.parent_id == Some(cur.id))
                .map(|s| StepView {
                    name: s.name.clone(),
                    title: s.title.clone(),
                    class: match s.state.as_str() {
                        "closed" => "done",
                        "active" => "now",
                        _ => "",
                    },
                })
                .collect(),
            columns: vec![
                col("To do", &["todo"]),
                col("In progress", &["doing"]),
                col("In review", &["review"]),
                col("Blocked or parked", &["blocked", "parked"]),
                col("Done", &["done"]),
            ],
        }
    });

    let row = |r: &Record| {
        let sub = match r.kind {
            Kind::Decision => format!(
                "{} · {}",
                tz.date(r.data.get("said_at").and_then(Value::as_i64).unwrap_or(r.created_at)),
                r.state
            ),
            Kind::Question if r.state == "answered" => format!("answered {}", ago(r.updated_at, now)),
            Kind::Question => format!("asked {}", ago(r.created_at, now)),
            _ => format!("{} · {}", r.state, ago(r.updated_at, now)),
        };
        RecordRow::new(r, sub)
    };
    // Decisions by when the owner said them, newest first.
    let mut decisions_all: Vec<&Record> = all.iter().filter(|r| r.kind == Kind::Decision).collect();
    decisions_all
        .sort_by_key(|r| std::cmp::Reverse(r.data.get("said_at").and_then(Value::as_i64).unwrap_or(r.created_at)));
    let mut questions_all: Vec<&Record> = all.iter().filter(|r| r.kind == Kind::Question).collect();
    questions_all.sort_by_key(|r| std::cmp::Reverse(r.number));

    let decisions_archived: Vec<RecordRow> = if tab == "decisions" {
        decisions_all.iter().filter(|r| r.state == "archived").map(|r| row(r)).collect()
    } else {
        Vec::new()
    };
    let decisions: Vec<RecordRow> = match tab {
        "decisions" => decisions_all.iter().filter(|r| r.state != "archived").map(|r| row(r)).collect(),
        _ => decisions_all.iter().filter(|r| r.state == "current").take(5).map(|r| row(r)).collect(),
    };
    let open_questions = questions_all.iter().filter(|r| r.state == "open").map(|r| row(r)).collect();
    let questions = if tab == "questions" { questions_all.iter().map(|r| row(r)).collect() } else { Vec::new() };

    // "1 works · 1 problem · 1 skipped" per build, from its feedback records.
    let verdicts: HashMap<i64, String> = if tab == "builds" {
        let mut by: HashMap<i64, (usize, usize, usize)> = HashMap::new();
        for f in all.iter().filter(|f| f.kind == Kind::Feedback) {
            let Some(b) = all.iter().find(|b| b.kind == Kind::Build && b.ref_id == f.str("build")) else { continue };
            let e = by.entry(b.id).or_default();
            match f.state.as_str() {
                "works" => e.0 += 1,
                "problem" => e.1 += 1,
                "skipped" => e.2 += 1,
                _ => {}
            }
        }
        by.into_iter()
            .map(|(id, (w, p, s))| {
                let mut parts = Vec::new();
                if w > 0 {
                    parts.push(format!("{w} works"));
                }
                if p > 0 {
                    parts.push(plural(p as i64, "problem", "problems"));
                }
                if s > 0 {
                    parts.push(format!("{s} skipped"));
                }
                (id, parts.join(" · "))
            })
            .filter(|(_, s)| !s.is_empty())
            .collect()
    } else {
        HashMap::new()
    };
    let (build_server, servers) = if tab == "settings" {
        (
            crate::remote::project_server(c, p.id)?.unwrap_or_default(),
            crate::remote::load_servers(c)?.into_iter().map(|b| b.name).collect(),
        )
    } else {
        (String::new(), Vec::new())
    };
    let edit = (tab == "settings").then(|| EditView {
        build_server,
        servers,
        name: p.name.clone(),
        description: p.description.clone(),
        repo: p.repo.clone(),
        checkout: p.checkout.clone(),
        machine: p.machine.clone(),
        model: p.model.clone(),
        effort: p.effort.clone(),
        ports: p.ports.join("\n"),
        build_product: p.build_product.clone(),
        merge_method: p.merge_method.clone(),
    });
    let shots: Vec<ShotCard> = if tab == "screenshots" {
        let mut list: Vec<&Record> =
            all.iter().filter(|r| r.kind == Kind::Screenshot && r.state != "withdrawn").collect();
        list.sort_by_key(|r| std::cmp::Reverse(r.number));
        list.iter()
            .map(|r| ShotCard {
                url: r.url(),
                image: format!("{}/image", r.url()),
                title: r.title.clone(),
                category: r.str("category").to_owned(),
                when: format!("{} · {}", r.short_id(), ago(r.created_at, now)),
                mstate: r.str("mockup_state").to_owned(),
                design: !r.str("design_url").is_empty(),
            })
            .collect()
    } else {
        Vec::new()
    };
    let builds = if tab == "builds" {
        let mut list: Vec<&Record> = all.iter().filter(|r| r.kind == Kind::Build).collect();
        list.sort_by_key(|r| std::cmp::Reverse(r.number));
        list.iter()
            .map(|r| {
                let waiting = r.data.get("ready_for_feedback").and_then(Value::as_bool).unwrap_or(false);
                let mut sub = format!("{} · {}", r.str("name"), ago(r.created_at, now));
                if waiting {
                    sub.push_str(" · waiting for your feedback");
                }
                if let Some(v) = verdicts.get(&r.id) {
                    sub.push_str(&format!(" · {v}"));
                }
                RecordRow::new(r, sub)
            })
            .collect()
    } else {
        Vec::new()
    };

    let groups = if tab == "tasks" {
        let line = |i: usize, t: &Record, next: bool, times: &crate::times::Times| -> anyhow::Result<TaskLine> {
            let blocked_by = if t.state == "blocked" {
                crate::steps::blocking_questions(c, t.id)?.into_iter().map(|(s, _, u)| (s, u)).collect()
            } else {
                Vec::new()
            };
            Ok(TaskLine {
                n: i + 1,
                short: t.short_id(),
                url: t.url(),
                title: t.title.clone(),
                state: t.state.clone(),
                est: t.str("estimate").to_owned(),
                next,
                blocked_by,
                blocked_note: if t.state == "blocked" { t.str("blocked_note").to_owned() } else { String::new() },
                added: if t.data.get("added").and_then(Value::as_bool).unwrap_or(false) {
                    let from = t.str("source_ref");
                    let src = source_label(t.str("source"));
                    if from.is_empty() { format!("added · {src}") } else { format!("added · {src} · from {from}") }
                } else {
                    String::new()
                },
                skip_reason: if t.state == "skipped" { t.str("skip_reason").to_owned() } else { String::new() },
                spent: times.share_ms.get(&t.id).filter(|ms| **ms > 0).map(|ms| crate::times::fmt_ms(*ms)).unwrap_or_default(),
            })
        };
        let lines = |list: &[&Record], active: bool, times: &crate::times::Times| -> anyhow::Result<Vec<TaskLine>> {
            let next = if active { list.iter().position(|t| t.state == "todo") } else { None };
            list.iter().enumerate().map(|(i, t)| line(i, t, Some(i) == next, times)).collect()
        };
        let est_of = |list: &[&Record]| -> i64 {
            list.iter().filter(|t| t.state != "skipped").map(|t| crate::times::estimate_min(t)).sum()
        };
        let mut sorted: Vec<&Record> = tasks.clone();
        sorted.sort_by(|a, b| {
            a.position.unwrap_or(f64::MAX).partial_cmp(&b.position.unwrap_or(f64::MAX)).unwrap_or(std::cmp::Ordering::Equal)
        });
        let of = |id: i64| -> Vec<&Record> { sorted.iter().copied().filter(|t| t.milestone_id == Some(id)).collect() };
        let mut groups = Vec::new();
        for m in ms.iter().filter(|m| m.parent_id.is_none()) {
            let steps: Vec<&Milestone> = ms.iter().filter(|s| s.parent_id == Some(m.id)).collect();
            let own = of(m.id);
            let count: usize = own.len() + steps.iter().map(|s| of(s.id).len()).sum::<usize>();
            if count == 0 && steps.is_empty() && m.state != "active" {
                continue;
            }
            let mut boxes = Vec::new();
            let mut agent_ms = 0;
            let mut est_all = est_of(&own);
            for st in &steps {
                let mine = of(st.id);
                let counted: Vec<&Record> = mine.iter().copied().filter(|t| t.state != "skipped").collect();
                let times = crate::times::times_of(c, &p.slug, &counted, now)?;
                agent_ms += times.total_ms;
                let est = est_of(&mine);
                est_all += est;
                let open = mine.iter().filter(|t| crate::steps::OPEN.contains(&t.state.as_str())).count();
                let blocked = mine.iter().filter(|t| t.state == "blocked").count();
                let state = match st.state.as_str() {
                    "closed" => "concluded",
                    "active" if open > 0 && blocked == open => "blocked",
                    "active" => "active",
                    _ => "planned",
                };
                let n = mine.len();
                let est_txt = if est > 0 { format!("est. {}", crate::times::fmt_min(est)) } else { String::new() };
                let sub = match state {
                    "concluded" => format!("{est_txt} · took {}", crate::times::fmt_ms(times.total_ms)),
                    "planned" if n == 0 => "starts when its first task does".into(),
                    "planned" => format!("{n} task{} · {est_txt}", if n == 1 { "" } else { "s" }),
                    _ => format!("{open} open of {n} · {est_txt} · spent {} so far", crate::times::fmt_ms(times.total_ms)),
                };
                let sub = sub.trim_start_matches(" · ").trim_end_matches(" · ").replace(" ·  · ", " · ");
                let review = !st.review.is_empty();
                let sub = if review {
                    let r: Value = serde_json::from_str(&st.review).unwrap_or_default();
                    let n = r["reviewers"].as_array().map(Vec::len).unwrap_or(0);
                    format!("{n} reviewer{} · commit {} · {sub}", if n == 1 { "" } else { "s" }, r["commit"].as_str().unwrap_or(""))
                } else {
                    sub
                };
                boxes.push(StepBox {
                    name: st.name.clone(),
                    title: st.title.clone(),
                    review,
                    state,
                    sub,
                    url: format!("/p/{}/steps/{}", p.slug, st.id),
                    rows: if state == "concluded" { Vec::new() } else { lines(&mine, st.state == "active", &times)? },
                });
            }
            let own_counted: Vec<&Record> = own.iter().copied().filter(|t| t.state != "skipped").collect();
            let own_times = crate::times::times_of(c, &p.slug, &own_counted, now)?;
            agent_ms += own_times.total_ms;
            let mut sub = match (steps.len(), count) {
                (0, n) => format!("{n} task{}", if n == 1 { "" } else { "s" }),
                (k, n) => format!("{k} step{} · {n} task{}", if k == 1 { "" } else { "s" }, if n == 1 { "" } else { "s" }),
            };
            if est_all > 0 {
                sub.push_str(&format!(" · estimated {}", crate::times::fmt_min(est_all)));
            }
            if agent_ms > 0 {
                sub.push_str(&format!(" · {} of agent time", crate::times::fmt_ms(agent_ms)));
            }
            let sub = sub;
            groups.push(TaskGroup {
                name: m.name.clone(),
                title: m.title.clone(),
                state: m.state.clone(),
                sub,
                rows: lines(&own, m.state == "active" && steps.is_empty(), &own_times)?,
                steps: boxes,
            });
        }
        let loose: Vec<&Record> = sorted.iter().copied().filter(|t| t.milestone_id.is_none()).collect();
        if !loose.is_empty() {
            groups.push(TaskGroup {
                name: String::new(),
                title: "Without a milestone".into(),
                state: String::new(),
                sub: format!("{} tasks", loose.len()),
                rows: lines(&loose, false, &crate::times::Times::default())?,
                steps: Vec::new(),
            });
        }
        groups
    } else {
        Vec::new()
    };

    let (external, external_done) = external_rows(c, &all, now)?;
    let to_test = if tab == "to-test" {
        Some(super::totest::view(
            c,
            p.id,
            &p.slug,
            super::totest::Place::Tab,
            format!("/p/{}?tab=to-test", p.slug),
            true,
            now,
        )?)
    } else {
        None
    };
    Ok(ProjectPage {
        to_test_open: crate::totest::open_count(c, p.id)?,
        doc: None,
        notes: None,
        to_test,
        external,
        external_done,
        session_url: crate::usage::session_link(c, p.id)?.map(|(u, _)| u),
        nav: Nav::new(&Actor::Anonymous, ""),
        connect: None,
        slug: p.slug.clone(),
        name: p.name.clone(),
        repo: p.repo.clone(),
        status: p.status.clone(),
        headline,
        tab: tab.into(),
        about,
        milestones,
        board,
        agent: AgentCard {
            session: if p.agent.session.is_empty() { "–".into() } else { p.agent.session.clone() },
            machine: if p.agent.machine.is_empty() { "–".into() } else { p.agent.machine.clone() },
            model: [p.agent.model.as_str(), p.agent.effort.as_str()]
                .iter()
                .filter(|s| !s.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(" · "),
            // Its own note if it set one, else the tasks in doing (Imaginarium's agent, 2026-11-12).
            working_on: if !p.agent.note.trim().is_empty() {
                p.agent.note.clone()
            } else {
                let mut doing: Vec<&Record> = all.iter().filter(|r| r.kind == Kind::Task && r.state == "doing").collect();
                doing.sort_by(|a, b| a.position.unwrap_or(f64::MAX).partial_cmp(&b.position.unwrap_or(f64::MAX)).unwrap_or(std::cmp::Ordering::Equal));
                match doing.as_slice() {
                    [] => String::new(),
                    [one] => format!("doing {} {}", one.short_id(), one.title),
                    [first, rest @ ..] => format!("doing {} {} (+{})", first.short_id(), first.title, rest.len()),
                }
            },
            seen: p.agent.seen_at.map(|t| ago(t, now)).unwrap_or_else(|| "never".into()),
            online,
            tokens_week: crate::usage::short(week.iter().sum()),
            strip: {
                let max = week.iter().copied().max().unwrap_or(0).max(1);
                week.iter()
                    .enumerate()
                    .map(|(i, n)| {
                        let h = if *n == 0 { 0 } else { ((*n * 100 / max) as u32).max(4) };
                        (h, format!("{}: {} new tokens", tz.date(week_days[i]), crate::usage::short(*n)))
                    })
                    .collect()
            },
        },
        open_questions,
        decisions,
        decisions_archived,
        groups,
        questions,
        builds,
        edit,
        shots_all: shots.len(),
        shots,
        shot_cats: Vec::new(),
        prs_total: 0,
        has_stack_button: false,
        tabs: TABS,
        // Merge order and releases appear on the overview and the Pull requests tab only.
        prs: if matches!(tab, "overview" | "prs") { super::buttons::pr_rows(c, p.id, now)? } else { Vec::new() },
        releases: if matches!(tab, "overview" | "prs") {
            super::buttons::release_rows(c, p.id, now)?
        } else {
            Vec::new()
        },
        notice: None,
        notice_bad: false,
        next: String::new(),
    })
}

// ---------------------------------------------------------------------------------------------
// A record's page

struct LinkGroup {
    label: String,
    items: Vec<(RecordRow, bool)>,
}

struct OptionView {
    n: usize,
    title: String,
    detail: String,
    recommended: bool,
    chosen: bool,
}

pub struct TimeView {
    spent: String,
    est: String,
    pct: i64,
    over: bool,
    raw: String,
    spans: String,
    corrected: bool,
}

struct HistoryRow {
    rev: i64,
    at: String,
    text: String,
}

#[derive(Template)]
#[template(path = "record.html")]
struct RecordPage {
    nav: Nav,
    session_url: Option<String>,
    /// A draft for someone else's repository, with Send, Ask for changes and Drop.
    draft: Option<super::outgoing::DraftView>,
    draft_notice: Option<(bool, String)>,
    /// The To test checklist, on a release's or a build's page.
    to_test: Option<super::totest::View>,
    project_slug: String,
    project_name: String,
    kind_label: &'static str,
    kind: &'static str,
    id: String,
    short: String,
    title: String,
    state: String,
    tags: Vec<String>,
    body_html: String,
    owner_words: String,
    words_line: String,
    options: Vec<OptionView>,
    answer: String,
    answer_line: String,
    can_answer: bool,
    /// "2: Icons with labels" when the agent goes ahead with that option until the owner answers.
    going_ahead: Option<String>,
    extra: Vec<(String, String)>,
    links: Vec<LinkGroup>,
    /// A question: the tasks it holds up, with their step (the owner, 2026-11-12: "The question
    /// should also link what its currently blocking").
    blocking: Vec<(RecordRow, String)>,
    /// A blocked task: the open questions it waits for, or the note on what blocks it.
    waits_for: Vec<RecordRow>,
    blocked_note: String,
    stale: Vec<RecordRow>,
    history: Vec<HistoryRow>,
    error: Option<String>,
    buttons: Vec<super::buttons::ButtonView>,
    pr_url: String,
    next: String,
    build: Option<BuildView>,
    /// A task's time spent against its estimate.
    time: Option<TimeView>,
    /// Builds and tests timed on this task or PR.
    timings: Vec<crate::times::TimingRow>,
    /// A note: its text and its file.
    note: Option<super::notes::NotePage>,
    /// A screenshot's image and its details.
    shot: Option<ShotView>,
    is_owner: bool,
}

/// A build's page: how to start it and what to try.
struct BuildView {
    run: String,
    /// "Linux · x86_64"; a build that isn't for flow-run is a download instead.
    platform: String,
    download: String,
    needs: Vec<String>,
    facts: Vec<(String, String)>,
    /// (index, title, what to try)
    features: Vec<(usize, String, String)>,
    /// Feedback already given: (feature, verdict, the owner's words)
    given: Vec<(String, String, String)>,
    can_feedback: bool,
    sent: bool,
}

#[derive(Deserialize)]
struct RecordQuery {
    error: Option<String>,
    /// Set after feedback on a build was sent.
    sent: Option<u8>,
    /// What happened to a draft for someone else's repository (sent, asked, dropped, …).
    draft: Option<String>,
    /// After Send on the To test list.
    tested: Option<usize>,
}

async fn record(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((slug, id)): Path<(String, String)>,
    Query(q): Query<RecordQuery>,
    tz: Tz,
) -> AppResult<Response> {
    // Only fixed messages come from the address, never text from it.
    let error = q.error.as_deref().map(|code| match code {
        "answered" => "This question was already answered.".to_owned(),
        "option" => "That option doesn't exist; pick one of the list.".to_owned(),
        "empty" => "Pick an option or write an answer.".to_owned(),
        "both" => "Pick an option or write your own answer, not both. Nothing was sent.".to_owned(),
        _ => "That didn't work; nothing was saved. Try again.".to_owned(),
    });
    let mut page = load_record_page(&state, &actor, slug, id, error, q.draft.clone(), tz).await?;
    if let Some(b) = page.build.as_mut() {
        b.sent = q.sent == Some(1);
    }
    if let Some(n) = q.tested {
        page.draft_notice = Some((true, tested_notice(n)));
    }
    render(page)
}

async fn load_record_page(
    state: &AppState,
    actor: &Actor,
    slug: String,
    id: String,
    error: Option<String>,
    draft_code: Option<String>,
    tz: Tz,
) -> AppResult<RecordPage> {
    let now = now_ms();
    let is_owner = actor.is_owner();
    let tz2 = tz.clone();
    let page = state
        .db
        .call(move |c| {
            // A malformed id is a page that doesn't exist; a database failure is an error.
            let Ok(rref) = records::RecordRef::parse(&format!("{slug}/{id}")) else { return Ok(None) };
            let Some(r) = records::try_find(c, &rref, None)? else { return Ok(None) };
            let project = projects::get_by_id(c, r.project_id)?;
            let links = records::links_of(c, &r)?;
            let history = records::revisions(c, r.id)?;
            let buttons = super::buttons::views(c, r.id, now)?;
            // A task's time (its share of its step's) and the timings on it or on a PR.
            let tz = &tz2;
            let time = if r.kind == Kind::Task {
                let siblings = match r.milestone_id {
                    Some(m) => crate::steps::tasks_of(c, m)?,
                    None => vec![r.clone()],
                };
                let counted: Vec<&Record> = siblings.iter().filter(|t| t.state != "skipped").collect();
                let times = crate::times::times_of(c, &project.slug, &counted, now)?;
                let share = times.share_ms.get(&r.id).copied().unwrap_or(0);
                let raw = times.raw_ms.get(&r.id).copied().unwrap_or(0);
                let est = crate::times::estimate_min(&r);
                let spans = crate::times::doing_spans(c, &r, now)?;
                Some(TimeView {
                    spent: crate::times::fmt_ms(share),
                    est: if est > 0 { crate::times::fmt_min(est) } else { String::new() },
                    pct: if est > 0 { ((share as f64 / (est as f64 * 60_000.0)) * 100.0).min(100.0) as i64 } else { 0 },
                    over: est > 0 && share > est * 60_000,
                    raw: if raw > share + 60_000 { format!("{} in doing; shared with tasks in doing at the same time", crate::times::fmt_ms(raw)) } else { String::new() },
                    spans: spans.iter().map(|(a, b)| if *b >= now - 1000 && r.state == "doing" { format!("{}–now", tz.short(*a)) } else { format!("{}–{}", tz.short(*a), tz.hm(*b)) }).collect::<Vec<_>>().join(", "),
                    corrected: r.data.get("time_override_min").is_some(),
                })
            } else {
                None
            };
            let timings = if matches!(r.kind, Kind::Task | Kind::Pr) { crate::times::rows(&crate::times::of_record(c, r.id)?, tz) } else { Vec::new() };
            let session_url = crate::usage::session_link(c, project.id)?.map(|(u, _)| u);
            let draft = super::outgoing::draft_view(c, &r, now)?;
            // Everything still to test, on a release that isn't tagged yet and on every build.
            let to_test = match (r.kind, r.state.as_str()) {
                (Kind::Release, "planned" | "waiting") => Some(super::totest::view(
                    c,
                    r.project_id,
                    &project.slug,
                    super::totest::Place::Release(&r),
                    r.url(),
                    is_owner,
                    now,
                )?),
                (Kind::Build, "published" | "tried") => Some(super::totest::view(
                    c,
                    r.project_id,
                    &project.slug,
                    super::totest::Place::Build,
                    r.url(),
                    is_owner,
                    now,
                )?),
                _ => None,
            };
            let draft_notice = match &draft_code {
                Some(code) if r.kind == Kind::Contribution => super::outgoing::notice(c, &r, code)?,
                _ => None,
            };
            Ok(Some((r, project, links, history, buttons, (session_url, draft, draft_notice, to_test, time, timings))))
        })
        .await?
        .ok_or(AppError::NotFound)?;
    let (r, project, links, history, buttons, (session_url, draft, draft_notice, to_test, time, timings)) = page;

    let mut blocking = Vec::new();
    let mut waits_for = Vec::new();
    for l in links.iter().filter(|l| l.relation == "blocks") {
        if l.outgoing && r.kind == Kind::Question && l.other.kind == Kind::Task && l.other.state == "blocked" {
            let step = l.other.milestone.clone().unwrap_or_default();
            blocking.push((RecordRow::new(&l.other, l.other.state.clone()), step));
        }
        if !l.outgoing && r.kind == Kind::Task && r.state == "blocked" && l.other.kind == Kind::Question && l.other.state == "open" {
            waits_for.push(RecordRow::new(&l.other, "open".into()));
        }
    }
    let blocked_note = if r.state == "blocked" { r.str("blocked_note").to_owned() } else { String::new() };
    let mut groups: Vec<LinkGroup> = Vec::new();
    let mut stale = Vec::new();
    for l in &links {
        // A build's feedback is listed on the page already ("Your feedback").
        if r.kind == Kind::Build && l.other.kind == Kind::Feedback {
            continue;
        }
        if l.stale {
            stale.push(RecordRow::new(&l.other, format!("last changed {}", ago(l.other.updated_at, now))));
        }
        let item = (RecordRow::new(&l.other, l.other.state.clone()), l.other.project != r.project);
        match groups.iter_mut().find(|g| g.label == l.label) {
            Some(g) => g.items.push(item),
            None => groups.push(LinkGroup { label: l.label.clone(), items: vec![item] }),
        }
    }

    let options: Vec<OptionView> = r
        .data
        .get("options")
        .and_then(Value::as_array)
        .map(|opts| {
            let rec = r.data.get("recommended").and_then(Value::as_u64).unwrap_or(0) as usize;
            let chosen = r.data.get("answer_option").and_then(Value::as_u64).unwrap_or(0) as usize;
            opts.iter()
                .enumerate()
                .map(|(i, o)| OptionView {
                    n: i + 1,
                    title: o.get("title").and_then(Value::as_str).unwrap_or("").into(),
                    detail: o.get("detail").and_then(Value::as_str).unwrap_or("").into(),
                    recommended: rec == i + 1,
                    chosen: chosen == i + 1,
                })
                .collect()
        })
        .unwrap_or_default();

    let mut extra = Vec::new();
    match r.kind {
        Kind::Adr => {
            let (repo, path) = (r.str("repo"), r.str("path"));
            if !repo.is_empty() && !path.is_empty() {
                extra.push(("File".into(), format!("https://github.com/{repo}/blob/main/{path}")));
            }
        }
        Kind::Task => {
            if let Some(m) = &r.milestone {
                extra.push(("Milestone".into(), m.clone()));
            }
            for (k, label) in [("estimate", "Estimate"), ("resurface_when", "Comes back"), ("importance", "Importance"), ("skip_reason", "Skipped because")] {
                if !r.str(k).is_empty() {
                    extra.push((label.into(), r.str(k).into()));
                }
            }
            if !r.str("source").is_empty() && r.str("source") != "planned" {
                let from = r.str("source_ref");
                let added = if r.data.get("added").and_then(Value::as_bool).unwrap_or(false) { "added during the step: " } else { "" };
                let src = source_label(r.str("source"));
                extra.push(("Came from".into(), if from.is_empty() { format!("{added}{src}") } else { format!("{added}{src} ({from})") }));
            }
            if !r.str("branch").is_empty() {
                extra.push(("Branch".into(), r.str("branch").into()));
            }
        }
        Kind::Release => {
            extra.push(("Tag".into(), r.str("tag").into()));
            if let Some(waits) = r.data.get("waits_for").and_then(Value::as_array) {
                let list: Vec<String> = waits
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|w| w.rsplit('/').next().unwrap_or(w).replace("PR-", "#"))
                    .collect();
                if !list.is_empty() {
                    extra.push(("Waits for".into(), list.join(", ")));
                }
            }
        }
        _ => {}
    }

    let words_line = if r.kind == Kind::Decision {
        let said = r.data.get("said_at").and_then(Value::as_i64).map(|t| tz.full(t)).unwrap_or_default();
        let answers = links
            .iter()
            .find(|l| l.relation == "answers" && l.outgoing)
            .map(|l| format!(" · answer to {}", l.other.ref_id))
            .unwrap_or_default();
        format!("{}{answers} · {} · recorded by {}", said.get(..16).unwrap_or(&said), r.str("source"), r.author)
    } else {
        String::new()
    };
    let answer_line = match r.data.get("answered_at").and_then(Value::as_i64) {
        Some(t) => format!("answered {} ({})", ago(t, now), tz.full(t).get(..16).unwrap_or("")),
        None => String::new(),
    };

    Ok(RecordPage {
        nav: nav(state, actor, "/projects").await.back_to(
            // A screenshot goes back to the Screenshots it was opened from.
            if r.kind == Kind::Screenshot {
                format!("/p/{}?tab=screenshots", project.slug)
            } else {
                format!("/p/{}", project.slug)
            },
            project.name.clone(),
        ),
        session_url,
        // A text note's body is its text, shown in its own section.
        body_html: if draft.is_some() || (r.kind == Kind::Note && r.str("ext").is_empty()) { String::new() } else { markdown::to_html(&r.body) },
        draft,
        draft_notice,
        to_test,
        project_slug: project.slug,
        project_name: project.name,
        kind_label: r.kind.label(),
        kind: r.kind.as_str(),
        id: r.ref_id.clone(),
        short: r.short_id(),
        title: r.title.clone(),
        state: r.state.clone(),
        tags: r.tags.clone(),
        owner_words: r.str("owner_words").into(),
        words_line,
        answer: r.str("answer").into(),
        answer_line,
        can_answer: is_owner && r.kind == Kind::Question && r.state == "open",
        going_ahead: (r.kind == Kind::Question && r.state == "open" && crate::waiting::is_going_ahead(&r)).then(|| {
            let n = r.data.get("recommended").and_then(Value::as_u64).unwrap_or(0) as usize;
            let title = r
                .data
                .get("options")
                .and_then(Value::as_array)
                .and_then(|o| o.get(n.wrapping_sub(1)))
                .and_then(|o| o.get("title"))
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("{n}: {title}")
        }),
        options,
        extra,
        links: groups,
        blocking,
        waits_for,
        blocked_note,
        stale,
        time,
        timings,
        note: if r.kind == Kind::Note { Some(super::notes::page(&state.config.data_dir(), &r)) } else { None },
        history: history
            .iter()
            .map(|h| HistoryRow { rev: h.rev, at: tz.short(h.at), text: describe_revision(h, &r) })
            .collect(),
        error,
        buttons,
        pr_url: if r.kind == Kind::Pr { r.str("url").to_owned() } else { String::new() },
        build: (r.kind == Kind::Build).then(|| build_view(&r, &links, false)),
        shot: (r.kind == Kind::Screenshot).then(|| ShotView {
            image: format!("{}/image", r.url()),
            category: r.str("category").to_owned(),
            line: {
                let size = r.data.get("size").and_then(Value::as_i64).unwrap_or(0);
                let dims =
                    match (r.data.get("width").and_then(Value::as_i64), r.data.get("height").and_then(Value::as_i64)) {
                        (Some(w), Some(h)) => format!("{w} × {h} · "),
                        _ => String::new(),
                    };
                format!("{dims}{} KB · uploaded by {} {}", (size + 1023) / 1024, r.author, ago(r.created_at, now))
            },
            mstate: r.str("mockup_state").to_owned(),
            design_url: r.str("design_url").to_owned(),
            state_action: format!("{}/mockup-state", r.url()),
        }),
        is_owner,
        next: r.url(),
    })
}

fn build_view(r: &Record, links: &[records::LinkView], sent: bool) -> BuildView {
    let size = r.data.get("size").and_then(Value::as_u64).unwrap_or(0);
    let mut facts = vec![("Build".to_owned(), format!("#{} · {}", r.number, r.str("name")))];
    if !r.str("commit").is_empty() {
        facts.push(("Commit".into(), format!("{} on {}", r.str("commit"), r.str("branch"))));
    }
    let size = if size >= 1_048_576 {
        format!("{:.1} MB", size as f64 / 1_048_576.0)
    } else {
        format!("{} KB", size.div_ceil(1024))
    };
    facts.push(("Size".into(), size));
    facts.push(("Starts".into(), r.str("entry").into()));
    let platform = if r.str("platform").is_empty() { crate::builds::DEFAULT_PLATFORM } else { r.str("platform") };
    if !r.str("tag").is_empty() {
        facts.push(("Tag".into(), r.str("tag").into()));
    }
    let features = r
        .data
        .get("features")
        .and_then(Value::as_array)
        .map(|fs| {
            fs.iter()
                .enumerate()
                .map(|(i, f)| {
                    let s = |k: &str| f.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
                    (i, s("title"), s("what_to_try"))
                })
                .collect()
        })
        .unwrap_or_default();
    let given = links
        .iter()
        .filter(|l| l.relation == "feedback_on" && !l.outgoing)
        .map(|l| (l.other.title.clone(), l.other.state.clone(), l.other.str("owner_words").to_owned()))
        .collect();
    BuildView {
        run: format!("flow-run {} {}", r.str("product"), r.number),
        platform: crate::builds::platform_label(platform),
        download: if platform == crate::builds::DEFAULT_PLATFORM { String::new() } else { format!("{}/download", r.url()) },
        needs: r.data.get("needs").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default(),
        facts,
        features,
        given,
        // No feedback form any more (the owner, 2026-11-11: "i can give general feedback to the
        // agent via chat"): what to try is in the To test list below Try it.
        can_feedback: false,
        sent,
    }
}

fn describe_revision(h: &records::Revision, r: &Record) -> String {
    let who = if h.author == "owner" { "You".to_owned() } else { h.author.clone() };
    if h.rev == 1 {
        return format!("{who} created the {}.", r.kind.label().to_lowercase());
    }
    // The owner's answer reads as the answer, not as a list of fields.
    if let Some(obj) = h.changes.as_object()
        && obj.contains_key("answer")
    {
        let words = obj.get("answer").and_then(Value::as_str).unwrap_or("").trim();
        let option = obj.get("answer_option_title").and_then(Value::as_str);
        return match (option, words) {
            (Some(o), "") => format!("{who} answered: {o}."),
            (Some(o), w) => format!("{who} answered: {o}, “{w}”."),
            (None, w) => format!("{who} answered: “{w}”."),
        };
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(obj) = h.changes.as_object() {
        for (k, v) in obj {
            parts.push(match (k.as_str(), v) {
                ("state", Value::String(s)) => format!("set it to {s}"),
                ("milestone", Value::String(m)) if !m.is_empty() => format!("moved it to {m}"),
                ("answer", Value::String(_))
                | ("answered_at", _)
                | ("answer_option", _)
                | ("answer_option_title", _) => continue,
                ("started_at", _) => continue,
                // Set by the feedback itself, which the next line of the history says.
                ("ready_for_feedback", _) => continue,
                (k, _) => format!("changed the {}", k.replace('_', " ")),
            });
        }
    }
    if !h.note.is_empty() {
        parts.push(h.note.clone());
    }
    if parts.is_empty() { format!("{who} changed it.") } else { format!("{who}: {}.", parts.join(", ")) }
}

#[derive(Deserialize)]
struct AnswerForm {
    option: Option<String>,
    words: Option<String>,
    /// Where to go back to (the dashboard or the phone inbox); only local paths.
    next: Option<String>,
}

async fn answer(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Extension(ip): Extension<ClientIp>,
    Path((slug, id)): Path<(String, String)>,
    Form(form): Form<AnswerForm>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let option = match form.option.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Some(s.parse::<u32>().map_err(|_| AppError::BadRequest("Pick one of the options.".into()))?),
        None => None,
    };
    let words = form.words.unwrap_or_default();
    // An option or the owner's own words, never both (the owner, 2026-11-11).
    if option.is_some() && !words.trim().is_empty() {
        return Ok(Redirect::to(&format!("/p/{slug}/{id}?error=both")).into_response());
    }
    let (s, i) = (slug.clone(), id.clone());
    let result = state
        .db
        .call(move |c| {
            let tx = c.transaction()?;
            let q = records::find_str(&tx, &format!("{s}/{i}"), None)?;
            // The answer stays on the question; the agent records a decision only when it thinks
            // the answer is one (the owner, 2026-11-12).
            let q = records::answer_question(&tx, &q, option, &words, "owner")?;
            tx.commit()?;
            Ok(q)
        })
        .await;
    match result {
        Ok(q) => {
            let e = Entry::new(&actor, Via::Page, Action::Write, format!("answered {}", q.ref_id))
                .project(q.project.clone())
                .target(q.ref_id.clone())
                .ip(Some(ip.0));
            activity::write(&state.db, e).await;
            // Back to the Dashboard: it says where the answer went and shows the next question.
            let next = super::local_path(form.next.as_deref()).unwrap_or_else(|| format!("/?answered={}", q.ref_id));
            Ok(Redirect::to(&next).into_response())
        }
        Err(e) => {
            // Back to the question with the reason, so reloading doesn't send the form again.
            let msg = format!("{e:#}");
            let code = if msg.contains("already") {
                "answered"
            } else if msg.contains("no option") {
                "option"
            } else if msg.contains("Pick an option") {
                "empty"
            } else {
                tracing::warn!("answering {slug}/{id}: {msg}");
                "failed"
            };
            Ok(Redirect::to(&format!("/p/{slug}/{id}?error={code}")).into_response())
        }
    }
}
