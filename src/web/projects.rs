//! The Projects list, a project's page (overview, tasks, decisions, questions) and a record's page.

use std::collections::HashMap;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::{Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Form, Router};
use rusqlite::Connection;
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
use crate::util::{ago, fmt_utc, now_ms};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/projects", get(list))
        .route("/p/{slug}", get(project))
        .route("/p/{slug}/{id}", get(record))
        .route("/p/{slug}/{id}/answer", post(answer))
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

fn open_question_counts(c: &Connection) -> anyhow::Result<HashMap<i64, i64>> {
    let mut stmt = c.prepare(
        "SELECT project_id, count(*) FROM records WHERE kind = 'question' AND state = 'open' GROUP BY project_id",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
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
    questions: String,
    status: String,
}

#[derive(Template)]
#[template(path = "projects.html")]
struct ProjectsPage {
    nav: Nav,
    cards: Vec<ProjectCard>,
}

async fn list(State(state): State<AppState>, Extension(actor): Extension<Actor>) -> AppResult<Response> {
    let now = now_ms();
    let cards = state
        .db
        .call(move |c| {
            let counts = open_question_counts(c)?;
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
                    questions: plural(counts.get(&p.id).copied().unwrap_or(0), "open question", "open questions"),
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
    render(ProjectsPage { nav: nav(&state, &actor, "/projects").await, cards })
}

fn model_line(p: &Project) -> String {
    match (p.model.as_str(), p.effort.as_str()) {
        ("", "") => String::new(),
        (m, "") => m.into(),
        ("", e) => format!("effort {e}"),
        (m, e) => format!("{m} · {e}"),
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

struct AgentCard {
    session: String,
    machine: String,
    model: String,
    working_on: String,
    seen: String,
    online: bool,
}

#[derive(Template)]
#[template(path = "project.html")]
struct ProjectPage {
    nav: Nav,
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
    prs: Vec<super::buttons::PrRow>,
    prs_total: usize,
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
}

/// The line shown after pressing a button, from the redirect's query.
fn press_notice(q: &TabQuery) -> (Option<String>, bool) {
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

const TABS: &[(&str, &str)] = &[
    ("overview", "Overview"),
    ("tasks", "Tasks"),
    ("prs", "Pull requests"),
    ("decisions", "Decisions"),
    ("questions", "Questions"),
];

async fn project(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(slug): Path<String>,
    Query(q): Query<TabQuery>,
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
            let all = records::query(c, &records::Query { project_id: Some(p.id), limit: 500, ..Default::default() })?;
            Ok(Some(build_project_page(c, p, ms, all, &t, now)?))
        })
        .await?
        .ok_or(AppError::NotFound)?;
    let mut page = page;
    page.prs_total = page.prs.len();
    if tab == "overview" {
        // The overview shows the start of the order; the Pull requests tab has all of them.
        page.prs.truncate(8);
    }
    let next = if tab == "overview" { format!("/p/{}", page.slug) } else { format!("/p/{}?tab={tab}", page.slug) };
    let page = ProjectPage { nav: nav(&state, &actor, "/projects").await, tab, notice, notice_bad, next, ..page };
    render(page)
}

fn build_project_page(
    c: &Connection,
    p: Project,
    ms: Vec<Milestone>,
    all: Vec<Record>,
    tab: &str,
    now: i64,
) -> anyhow::Result<ProjectPage> {
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
             WHERE l.relation = 'blocks' AND r.state IN ('open', 'todo', 'doing', 'blocked')",
        )?;
        let rows = stmt.query_map([], |r| {
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
        let ids: Vec<i64> =
            std::iter::once(cur.id).chain(ms.iter().filter(|s| s.parent_id == Some(cur.id)).map(|s| s.id)).collect();
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
                fmt_utc(r.data.get("said_at").and_then(Value::as_i64).unwrap_or(r.created_at)).get(..10).unwrap_or(""),
                r.state
            ),
            Kind::Question if r.state == "answered" => format!("answered {}", ago(r.updated_at, now)),
            Kind::Question => format!("asked {}", ago(r.created_at, now)),
            _ => format!("{} · {}", r.state, ago(r.updated_at, now)),
        };
        RecordRow::new(r, sub)
    };
    let mut decisions_all: Vec<&Record> = all.iter().filter(|r| r.kind == Kind::Decision).collect();
    decisions_all.sort_by_key(|r| std::cmp::Reverse(r.number));
    let mut questions_all: Vec<&Record> = all.iter().filter(|r| r.kind == Kind::Question).collect();
    questions_all.sort_by_key(|r| std::cmp::Reverse(r.number));

    let decisions: Vec<RecordRow> = match tab {
        "decisions" => decisions_all.iter().map(|r| row(r)).collect(),
        _ => decisions_all.iter().filter(|r| r.state == "current").take(5).map(|r| row(r)).collect(),
    };
    let open_questions = questions_all.iter().filter(|r| r.state == "open").map(|r| row(r)).collect();
    let questions = if tab == "questions" { questions_all.iter().map(|r| row(r)).collect() } else { Vec::new() };

    let groups = if tab == "tasks" {
        let mut groups = Vec::new();
        for m in ms.iter().filter(|m| m.parent_id.is_none()) {
            let ids: Vec<i64> =
                std::iter::once(m.id).chain(ms.iter().filter(|s| s.parent_id == Some(m.id)).map(|s| s.id)).collect();
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

    Ok(ProjectPage {
        nav: Nav::new(&Actor::Anonymous, ""),
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
        },
        open_questions,
        decisions,
        groups,
        questions,
        prs_total: 0,
        prs: super::buttons::pr_rows(c, p.id, now)?,
        releases: super::buttons::release_rows(c, p.id, now)?,
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
}

async fn record(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path((slug, id)): Path<(String, String)>,
) -> AppResult<Response> {
    let page = load_record_page(&state, &actor, slug, id, None).await?;
    render(page)
}

async fn load_record_page(
    state: &AppState,
    actor: &Actor,
    slug: String,
    id: String,
    error: Option<String>,
) -> AppResult<RecordPage> {
    let now = now_ms();
    let is_owner = actor.is_owner();
    let page = state
        .db
        .call(move |c| {
            let Ok(r) = records::find_str(c, &format!("{slug}/{id}"), None) else { return Ok(None) };
            let project = projects::get_by_id(c, r.project_id)?;
            let links = records::links_of(c, &r)?;
            let history = records::revisions(c, r.id)?;
            let buttons = super::buttons::views(c, r.id, now)?;
            Ok(Some((r, project, links, history, buttons)))
        })
        .await?
        .ok_or(AppError::NotFound)?;
    let (r, project, links, history, buttons) = page;

    let mut groups: Vec<LinkGroup> = Vec::new();
    let mut stale = Vec::new();
    for l in &links {
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
        _ => {}
    }

    let words_line = if r.kind == Kind::Decision {
        let said = r.data.get("said_at").and_then(Value::as_i64).map(fmt_utc).unwrap_or_default();
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
        Some(t) => format!("answered {} ({} UTC)", ago(t, now), fmt_utc(t).get(..16).unwrap_or("")),
        None => String::new(),
    };

    Ok(RecordPage {
        nav: nav(state, actor, "/projects").await,
        project_slug: project.slug,
        project_name: project.name,
        kind_label: r.kind.label(),
        kind: r.kind.as_str(),
        id: r.ref_id.clone(),
        short: r.short_id(),
        title: r.title.clone(),
        state: r.state.clone(),
        tags: r.tags.clone(),
        body_html: markdown::to_html(&r.body),
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
            .map(|h| HistoryRow {
                rev: h.rev,
                at: fmt_utc(h.at).get(5..16).unwrap_or("").into(),
                text: describe_revision(h, &r),
            })
            .collect(),
        error,
        buttons,
        pr_url: if r.kind == Kind::Pr { r.str("url").to_owned() } else { String::new() },
        next: r.url(),
    })
}

fn describe_revision(h: &records::Revision, r: &Record) -> String {
    let who = h.author.clone();
    if h.rev == 1 {
        return format!("{who} created the {}.", r.kind.label().to_lowercase());
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
            let next = form.next.filter(|n| n.starts_with('/') && !n.starts_with("//")).unwrap_or_else(|| q.url());
            Ok(Redirect::to(&next).into_response())
        }
        Err(e) => {
            let page = load_record_page(&state, &actor, slug, id, Some(format!("{e:#}"))).await?;
            render(page)
        }
    }
}

use axum::response::IntoResponse;
