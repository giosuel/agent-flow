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
    stack: String,
    platform: String,
    model: String,
    effort: String,
    pm_rules: String,
    git_rules: String,
    ports: String,
    build_product: String,
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
        stack: Some(f.stack.split(',').map(str::to_owned).collect()),
        platform: Some(f.platform.clone()),
        model: Some(f.model.clone()),
        effort: Some(f.effort.clone()),
        pm_rules: Some(lines(&f.pm_rules)),
        git_rules: Some(f.git_rules.clone()),
        ports: Some(lines(&f.ports)),
        build_product: Some(f.build_product.clone()),
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

/// "2 questions · 1 build to try", or "nothing waiting on you".
fn waiting_line((questions, builds): (i64, i64)) -> String {
    let mut parts = Vec::new();
    if questions > 0 {
        parts.push(plural(questions, "question", "questions"));
    }
    if builds > 0 {
        parts.push(plural(builds, "build to try", "builds to try"));
    }
    if parts.is_empty() { "nothing waiting on you".into() } else { parts.join(" · ") }
}

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

// ---------------------------------------------------------------------------------------------
// Projects list

struct ProjectCard {
    slug: String,
    name: String,
    description: String,
    online: bool,
    agent_state: &'static str,
    facts: Vec<String>,
    milestone: Option<(String, String)>,
    waiting: String,
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
}

async fn list(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<ListQuery>,
) -> AppResult<Response> {
    let now = now_ms();
    let archived = q.archived == Some(1);
    let cards: Vec<ProjectCard> = state
        .db
        .call(move |c| {
            let counts = waiting_counts(c)?;
            let mut cards = Vec::new();
            for p in projects::list(c)? {
                let ms = projects::milestones(c, p.id)?;
                let online = p.agent.online(now);
                let mut facts = Vec::new();
                if !p.stack.is_empty() {
                    facts.push(p.stack.join(" · "));
                }
                for f in [&p.platform, &model_line(&p), &p.machine] {
                    if !f.is_empty() {
                        facts.push(f.clone());
                    }
                }
                cards.push(ProjectCard {
                    online,
                    agent_state: if online {
                        "agent online"
                    } else if p.agent.seen_at.is_some() {
                        "agent idle"
                    } else {
                        "no agent yet"
                    },
                    milestone: current_milestone(&ms).map(|m| (m.name.clone(), m.title.clone())),
                    waiting: waiting_line(counts.get(&p.id).copied().unwrap_or((0, 0))),
                    facts,
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
    let cards = cards.into_iter().filter(|c| (c.status == "archived") == archived).collect();
    let page_nav = nav(&state, &actor, "/projects").await;
    render(ProjectsPage {
        nav: if archived { page_nav.back_to("/projects", "Projects") } else { page_nav },
        cards,
        archived,
        archived_count,
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

struct TaskGroup {
    name: String,
    title: String,
    state: String,
    rows: Vec<RecordRow>,
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
    stack: String,
    platform: String,
    model: String,
    effort: String,
    pm_rules: String,
    git_rules: String,
    ports: String,
    build_product: String,
}

pub struct ShotCard {
    url: String,
    image: String,
    title: String,
    category: String,
    when: String,
}

pub struct ShotView {
    image: String,
    category: String,
    line: String,
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
}

/// The line shown after pressing a button, from the redirect's query.
fn press_notice(q: &TabQuery) -> (Option<String>, bool) {
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
    ("screenshots", "Screenshots"),
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
    let tab = q.tab.filter(|t| TABS.iter().any(|(k, _)| k == t)).unwrap_or_else(|| "overview".into());
    let now = now_ms();
    let t = tab.clone();
    let page = state
        .db
        .call(move |c| {
            let Some(p) = projects::get(c, &slug)? else { return Ok(None) };
            let ms = projects::milestones(c, p.id)?;
            let all = records::query(c, &records::Query { project_id: Some(p.id), limit: 0, ..Default::default() })?;
            let checkout = p.checkout.trim().to_owned();
            Ok(Some((build_project_page(c, p, ms, all, &t, now, &tz)?, checkout)))
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

fn build_project_page(
    c: &Connection,
    p: Project,
    ms: Vec<Milestone>,
    all: Vec<Record>,
    tab: &str,
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
    meta("stack", "Tech stack", p.stack.join(" · "), vec![]);
    meta("platform", "Target platform", p.platform.clone(), vec![]);
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
    meta("git", "Git", p.git_rules.clone(), vec![]);
    meta("port", "Ports", p.ports.join(", "), vec![]);
    meta("rules", "PM rules for this project", String::new(), p.pm_rules.clone());

    let current = current_milestone(&ms).cloned();
    let milestones = ms
        .iter()
        .filter(|m| m.parent_id.is_none())
        .map(|m| {
            let is_current = current.as_ref().is_some_and(|c| c.id == m.id);
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

    let decisions: Vec<RecordRow> = match tab {
        "decisions" => decisions_all.iter().map(|r| row(r)).collect(),
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
        stack: p.stack.join(", "),
        platform: p.platform.clone(),
        model: p.model.clone(),
        effort: p.effort.clone(),
        pm_rules: p.pm_rules.join("\n"),
        git_rules: p.git_rules.clone(),
        ports: p.ports.join("\n"),
        build_product: p.build_product.clone(),
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
        let mut groups = Vec::new();
        for m in ms.iter().filter(|m| m.parent_id.is_none()) {
            let ids: Vec<i64> = projects::with_steps(&ms, m.id);
            let mut rows: Vec<&Record> =
                tasks.iter().copied().filter(|t| t.milestone_id.is_some_and(|x| ids.contains(&x))).collect();
            rows.sort_by(|a, b| a.position.partial_cmp(&b.position).unwrap_or(std::cmp::Ordering::Equal));
            if rows.is_empty() {
                continue;
            }
            groups.push(TaskGroup {
                name: m.name.clone(),
                title: m.title.clone(),
                state: m.state.clone(),
                rows: rows
                    .iter()
                    .map(|t| {
                        let step = step_name(t.milestone_id);
                        let sub = if step.is_empty() { t.state.clone() } else { format!("{step} · {}", t.state) };
                        RecordRow::new(t, sub)
                    })
                    .collect(),
            });
        }
        let loose: Vec<RecordRow> =
            tasks.iter().filter(|t| t.milestone_id.is_none()).map(|t| RecordRow::new(t, t.state.clone())).collect();
        if !loose.is_empty() {
            groups.push(TaskGroup {
                name: String::new(),
                title: "Without a milestone".into(),
                state: String::new(),
                rows: loose,
            });
        }
        groups
    } else {
        Vec::new()
    };

    let (external, external_done) = external_rows(c, &all, now)?;
    Ok(ProjectPage {
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
            working_on: p.agent.note.clone(),
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
    extra: Vec<(String, String)>,
    links: Vec<LinkGroup>,
    stale: Vec<RecordRow>,
    history: Vec<HistoryRow>,
    error: Option<String>,
    buttons: Vec<super::buttons::ButtonView>,
    pr_url: String,
    next: String,
    build: Option<BuildView>,
    /// A screenshot's image and its details.
    shot: Option<ShotView>,
    is_owner: bool,
}

/// A build's page: how to start it and what to try.
struct BuildView {
    run: String,
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
            let session_url = crate::usage::session_link(c, project.id)?.map(|(u, _)| u);
            let draft = super::outgoing::draft_view(c, &r, now)?;
            let draft_notice = match &draft_code {
                Some(code) if r.kind == Kind::Contribution => super::outgoing::notice(c, &r, code)?,
                _ => None,
            };
            Ok(Some((r, project, links, history, buttons, (session_url, draft, draft_notice))))
        })
        .await?
        .ok_or(AppError::NotFound)?;
    let (r, project, links, history, buttons, (session_url, draft, draft_notice)) = page;

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
            for (k, label) in [("estimate", "Estimate"), ("resurface_when", "Comes back")] {
                if !r.str(k).is_empty() {
                    extra.push((label.into(), r.str(k).into()));
                }
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
        body_html: if draft.is_some() { String::new() } else { markdown::to_html(&r.body) },
        draft,
        draft_notice,
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
        options,
        extra,
        links: groups,
        stale,
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
        run: format!("agentflow run {} {}", r.str("product"), r.number),
        facts,
        features,
        given,
        can_feedback: r.state == "published",
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
