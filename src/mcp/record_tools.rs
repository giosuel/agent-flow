//! Tools for records: tasks, questions, decisions, ADRs, links, lookups and urgent calls.

use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{FlowMcp, Outcome, actor_of, target_project, writable};
use crate::activity::Action;
use crate::projects;
use crate::push::{self, Notification};
use crate::records::{self, Kind, NewRecord, Patch, Query, Record};
use crate::util::{fmt_utc, now_ms};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddTask {
    /// Only for the consultant: which project.
    pub project: Option<String>,
    /// The step (or milestone) it belongs to, e.g. "P9.3d" or "M4". Every task belongs to one.
    pub milestone: String,
    #[serde(flatten)]
    pub task: TaskItem,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct TaskItem {
    pub title: String,
    /// Details, acceptance criteria, paths.
    #[serde(default)]
    pub body: String,
    /// Your estimate, e.g. "2 h", "40 min" or "1 h 30 min". Required (not for skipped): actual
    /// times are set against it when the step concludes. Tasks of 30 min to 3 h estimate well.
    pub estimate: Option<String>,
    /// Where it came from: planned (with its step), owner_feedback, found (while building), review
    /// or bug. Left out: planned before the step starts, found once it's under way.
    pub source: Option<String>,
    /// What it came from, e.g. "B-56", "N-6", "Q-12", or the step it follows up, "P9.t56".
    pub source_ref: Option<String>,
    /// For review findings: high, medium or low.
    pub importance: Option<String>,
    /// The branch its work is on, e.g. "feat/stroke-dash-gap"; its PR is linked by itself.
    pub branch: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// todo (default), doing, parked, or skipped (a finding you won't do; needs reason).
    pub state: Option<String>,
    /// Why it's skipped.
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddTasks {
    /// Only for the consultant: which project.
    pub project: Option<String>,
    /// The step (or milestone) they belong to, e.g. "P9.3d".
    pub milestone: String,
    /// The tasks, in the order you'll do them. All are added or none (with each problem listed).
    pub tasks: Vec<TaskItem>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct OrderTasks {
    /// Only for the consultant: which project.
    pub project: Option<String>,
    /// The step (or milestone).
    pub milestone: String,
    /// Tasks in the order you'll do them, e.g. ["T-8", "T-7"]; the step's other tasks follow in
    /// their current order.
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct UpdateTask {
    /// The task, e.g. "T-14" or "vinculum/T-14".
    #[serde(default)]
    pub id: String,
    /// Several tasks at once, e.g. the three one PR carries: the same change for each.
    #[serde(default)]
    pub ids: Vec<String>,
    /// todo, doing, review, done, blocked, parked or skipped. Set doing when you start it: that also
    /// starts its step. "blocked" needs blocked_by, "skipped" a note on why.
    pub state: Option<String>,
    /// What blocks it: the question (e.g. "Q-12") for anything that needs the owner, or a short
    /// note for anything internal ("the UX agent's review", "remote job 17"). Sets it blocked; an
    /// answer to the question unblocks it by itself.
    pub blocked_by: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    /// Move it to another step or milestone.
    pub milestone: Option<String>,
    pub tags: Option<Vec<String>>,
    /// For parked tasks: when it should come back, e.g. "after M5".
    pub resurface_when: Option<String>,
    /// A revised estimate, e.g. "3 h".
    pub estimate: Option<String>,
    /// The branch its work is on.
    pub branch: Option<String>,
    /// Correct the time spent when the counted one is wrong, e.g. "1 h 20 min"; needs a note on
    /// why. Kept in the history.
    pub time_spent: Option<String>,
    /// A line for the task's history; for skipped or a time correction, why.
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LogTiming {
    /// What ran, e.g. "cargo test --workspace".
    pub what: String,
    /// Wall time in seconds.
    pub wall_s: f64,
    /// CPU seconds (user + sys of the whole process tree), if you know them.
    pub cpu_s: Option<f64>,
    /// Peak memory in MB, if you know it.
    pub peak_mb: Option<i64>,
    /// Where it ran: "here" (default) or a build server's name.
    pub place: Option<String>,
    /// What it's on: a task (T-12), a step (P9.3d) or a PR (#58). Left out: the one task in
    /// doing, or the one step under way.
    pub on: Option<String>,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QuestionOption {
    /// Short, e.g. "A: vertical list".
    pub title: String,
    /// One sentence on the trade-off.
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AskQuestion {
    /// The question itself, e.g. "Settings sidebar: tabs A or B?". One question per call.
    pub title: String,
    /// Context the owner needs to answer without opening anything else.
    #[serde(default)]
    pub body: String,
    /// Numbered options, if it's a choice. The owner can also answer in their own words.
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    /// The option you recommend, counting from 1.
    pub recommended: Option<u32>,
    /// What this question blocks, e.g. ["T-18", "T-19"]: those tasks are set blocked and come back
    /// to todo by themselves when the owner answers.
    #[serde(default)]
    pub blocks: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Going ahead with your recommendation meanwhile: true when that choice is cheap to change.
    /// You build the recommended option now (easy to switch), and the owner confirms or picks
    /// another when they get to it; you hear which. Needs `recommended`. No notification, no
    /// badge: it waits quietly in its own group. Leave it false (the owner decides first) only
    /// for choices that are expensive to undo; then keep working on whatever doesn't depend on it.
    #[serde(default)]
    pub going_ahead: bool,
    /// Send it as an urgent notification. Only when waiting would really cost something.
    #[serde(default)]
    pub urgent: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddToTest {
    /// What to test, in a line, e.g. "Import a folder with 10,000 photos".
    pub title: String,
    /// How: what to do and what should happen, e.g. "Pick ~/Pictures/big in Import; it stays
    /// responsive and ends with 10,000 in All photos."
    #[serde(default)]
    pub how: String,
    /// A category of your choosing, a few words, e.g. "Import", "Albums", "Print and PDF". Keep
    /// using the same ones; the owner sees the list grouped by them.
    pub category: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ResolveTests {
    /// The items, e.g. ["TT-3", "TT-5"].
    pub ids: Vec<String>,
    /// "works" (tested, done), "problem" (stays open; say what in the note) or "dropped" (no
    /// longer needed; say why).
    pub result: String,
    /// The owner's words for a problem, or why it's dropped; optional for works.
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Withdraw {
    /// The question, e.g. "Q-12".
    pub id: String,
    /// Why it no longer needs an answer.
    pub reason: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecordDecision {
    /// Only for the consultant: which project.
    pub project: Option<String>,
    /// The decision as one statement, e.g. "Stacked PRs are always based on the previous PR".
    pub title: String,
    /// Why, and what it means in practice.
    #[serde(default)]
    pub body: String,
    /// The owner's words, verbatim. Never paraphrase them.
    pub owner_words: String,
    /// When they said it, e.g. "2026-11-06 14:12" (UTC). Now if left out.
    pub said_at: Option<String>,
    /// "owner" if they told you directly, or "relayed by <session>" if another session passed it on.
    pub source: String,
    /// The question this answers, e.g. "Q-29".
    pub answers: Option<String>,
    /// An older decision this replaces, e.g. "D-17".
    pub supersedes: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddAdr {
    /// Only for the consultant: which project.
    pub project: Option<String>,
    pub title: String,
    /// Repository, e.g. "giosuel/imaginarium".
    pub repo: String,
    /// The ADR's own number, e.g. 43.
    pub number: Option<i64>,
    /// Path in the repository, e.g. "docs/adr/0043-rendering.md".
    pub path: String,
    /// The decision it records, e.g. "D-22".
    pub records: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Link {
    /// e.g. "PR-58" or "vinculum/T-12"
    pub from: String,
    /// supersedes, answers, blocks, implements, records, depends_on, feedback_on or relates
    pub relation: String,
    pub to: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetRecord {
    /// e.g. "D-22" or "vinculum/D-22"
    pub id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FindRecords {
    /// A project's slug, or "all". Your own project if left out.
    pub project: Option<String>,
    /// question, decision, task, adr, pr, release, build, feedback (any if left out)
    pub kind: Option<String>,
    /// e.g. "open", "doing", "current"
    pub state: Option<String>,
    /// Tasks of this milestone or step, e.g. "M4".
    pub milestone: Option<String>,
    pub tag: Option<String>,
    /// Words to look for in titles, bodies and fields.
    pub text: Option<String>,
    /// Only records changed on or after this date, e.g. "2026-11-01".
    pub since: Option<String>,
    /// At most this many (default 50).
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NotifyOwner {
    /// What needs the owner, in a few words.
    pub title: String,
    /// One or two sentences: what happened and what you need.
    pub body: String,
    /// A record this is about, e.g. "PR-56".
    pub about: Option<String>,
}

fn brief(r: &Record) -> Value {
    let mut v = json!({"id": r.ref_id, "title": r.title, "state": r.state});
    if let Some(m) = &r.milestone {
        v["milestone"] = m.clone().into();
    }
    v
}

fn parse_date(s: &str) -> anyhow::Result<i64> {
    let s = s.trim();
    let f = time::macros::format_description!("[year]-[month]-[day]");
    let fm = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]");
    if let Ok(d) = time::PrimitiveDateTime::parse(s, &fm) {
        return Ok((d.assume_utc().unix_timestamp_nanos() / 1_000_000) as i64);
    }
    let d = time::Date::parse(s, &f).map_err(|_| anyhow::anyhow!("dates look like 2026-11-06 or 2026-11-06 14:12"))?;
    Ok((d.midnight().assume_utc().unix_timestamp_nanos() / 1_000_000) as i64)
}

fn check_branch(b: &str) -> anyhow::Result<()> {
    if b.is_empty() || b.len() > 120 || b.starts_with('-') || !b.chars().all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c)) {
        anyhow::bail!("a branch name is letters, digits and / . _ - (e.g. feat/stroke-dash-gap)");
    }
    Ok(())
}

/// Checks one task before it's added; the problem in words if it can't be.
fn check_item(t: &TaskItem) -> Result<(), String> {
    let state = t.state.as_deref().unwrap_or("todo");
    if !["todo", "doing", "parked", "skipped"].contains(&state) {
        return Err(format!("state {state:?}: add tasks as todo, doing, parked or skipped (block them afterwards with update_task)"));
    }
    if t.title.trim().is_empty() {
        return Err("a title".into());
    }
    if state == "skipped" {
        if t.reason.as_deref().map(str::trim).unwrap_or("").is_empty() {
            return Err("a reason it's skipped".into());
        }
    } else {
        match t.estimate.as_deref().map(str::trim) {
            None | Some("") => return Err("an estimate, e.g. \"2 h\" or \"40 min\"".into()),
            Some(e) if crate::steps::parse_minutes(e).is_none() => {
                return Err(format!("estimate {e:?} isn't a duration; write it like \"2 h\", \"40 min\" or \"1 h 30 min\""));
            }
            _ => {}
        }
    }
    if let Some(src) = &t.source
        && !crate::steps::SOURCES.contains(&src.as_str())
    {
        return Err(format!("source {src:?}: one of {}", crate::steps::SOURCES.join(", ")));
    }
    if let Some(i) = &t.importance
        && !["high", "medium", "low"].contains(&i.as_str())
    {
        return Err("importance is high, medium or low".into());
    }
    if let Some(b) = &t.branch {
        check_branch(b.trim()).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Adds one checked task to a step. Added while the step is under way (and not said to be
/// planned), it's marked added, with where it came from ("found" unless said).
fn add_one(tx: &rusqlite::Connection, project_id: i64, m: &projects::Milestone, t: TaskItem, actor: &str) -> anyhow::Result<(Record, Vec<String>)> {
    let state = t.state.clone().unwrap_or_else(|| "todo".into());
    let under_way = m.state == "active";
    let source = t.source.clone().unwrap_or_else(|| if under_way { "found".into() } else { "planned".into() });
    let mut data = Map::new();
    if let Some(e) = t.estimate.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
        data.insert("estimate".into(), e.into());
        if let Some(min) = crate::steps::parse_minutes(e) {
            data.insert("estimate_min".into(), min.into());
        }
    }
    data.insert("source".into(), source.clone().into());
    if under_way && source != "planned" {
        data.insert("added".into(), true.into());
    }
    for (k, v) in [("source_ref", &t.source_ref), ("importance", &t.importance), ("branch", &t.branch)] {
        if let Some(v) = v.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            data.insert(k.into(), v.into());
        }
    }
    if state == "skipped" {
        data.insert("skip_reason".into(), t.reason.as_deref().unwrap_or("").trim().into());
    }
    if state == "doing" {
        data.insert("started_at".into(), now_ms().into());
    }
    let r = records::create(
        tx,
        project_id,
        actor,
        NewRecord {
            kind: Some(Kind::Task),
            title: t.title,
            body: t.body,
            state: Some(state.clone()),
            milestone_id: Some(m.id),
            tags: t.tags,
            data,
            ..Default::default()
        },
    )?;
    let started = if state == "doing" { crate::steps::task_started(tx, &r, actor)? } else { Vec::new() };
    Ok((r, started))
}

/// One task's update (update_task applies it to each of `ids`).
fn update_one(tx: &rusqlite::Connection, a: &crate::auth::Actor, id: &str, p: &UpdateTask) -> anyhow::Result<(Record, bool, Vec<String>)> {
    let r = writable(tx, a, id)?;
    if r.kind != Kind::Task {
        anyhow::bail!("{} isn't a task", r.ref_id);
    }
    let milestone_id = match &p.milestone {
        Some(name) => {
            let m = projects::find_milestone(tx, r.project_id, name)?;
            if m.state == "closed" {
                anyhow::bail!("{} is concluded; move the task to a step that isn't", m.name);
            }
            Some(Some(m.id))
        }
        None => None,
    };
    let blocked_by = p.blocked_by.as_deref().map(str::trim).filter(|b| !b.is_empty()).map(str::to_owned);
    if p.state.as_deref() == Some("blocked") && blocked_by.is_none() {
        anyhow::bail!("say what blocks it in blocked_by: the question (Q-12) for anything that needs the owner, or a short note");
    }
    if blocked_by.is_some() && p.state.as_deref().is_some_and(|s| s != "blocked") {
        anyhow::bail!("blocked_by goes with state blocked (or leave state out)");
    }
    if p.state.as_deref() == Some("skipped") && p.note.trim().is_empty() {
        anyhow::bail!("say why it's skipped, in note");
    }
    // Blocking is its own change below; the rest of the patch first.
    let state = if blocked_by.is_some() { None } else { p.state.clone() };
    let mut data = Map::new();
    if let Some(w) = &p.resurface_when {
        data.insert("resurface_when".into(), if w.trim().is_empty() { Value::Null } else { w.trim().into() });
    }
    if let Some(e) = &p.estimate {
        let e = e.trim();
        if e.is_empty() {
            anyhow::bail!("an estimate stays; give the revised one, e.g. \"3 h\"");
        }
        let Some(min) = crate::steps::parse_minutes(e) else {
            anyhow::bail!("estimate {e:?} isn't a duration; write it like \"2 h\", \"40 min\" or \"1 h 30 min\"");
        };
        data.insert("estimate".into(), e.into());
        data.insert("estimate_min".into(), min.into());
    }
    if let Some(b) = &p.branch {
        let b = b.trim();
        if b.is_empty() {
            data.insert("branch".into(), Value::Null);
        } else {
            check_branch(b)?;
            data.insert("branch".into(), b.into());
        }
    }
    if state.as_deref() == Some("skipped") {
        data.insert("skip_reason".into(), p.note.trim().into());
    }
    if let Some(ts) = p.time_spent.as_deref().map(str::trim) {
        if ts.is_empty() {
            data.insert("time_override_min".into(), Value::Null);
        } else {
            let Some(min) = crate::steps::parse_minutes(ts) else {
                anyhow::bail!("time_spent {ts:?} isn't a duration; write it like \"1 h 20 min\"");
            };
            if p.note.trim().is_empty() {
                anyhow::bail!("say why the counted time is wrong, in note");
            }
            data.insert("time_override_min".into(), min.into());
        }
    }
    // Started now only if it wasn't already in progress.
    let to_doing = state.as_deref() == Some("doing") && r.state != "doing";
    if to_doing {
        data.insert("started_at".into(), now_ms().into());
    }
    // Leaving blocked drops the note on what blocked it.
    if state.as_deref().is_some_and(|s| s != "blocked") && r.state == "blocked" {
        data.insert("blocked_note".into(), Value::Null);
    }
    let (mut r, mut changed) = records::update(
        tx,
        &r,
        &a.name(),
        Patch {
            title: p.title.clone(),
            body: p.body.clone(),
            state,
            milestone_id,
            tags: p.tags.clone(),
            data,
            note: p.note.clone(),
            ..Default::default()
        },
    )?;
    if let Some(by) = &blocked_by {
        r = crate::steps::block(tx, &r, by, &a.name())?;
        changed = true;
    }
    let started = if to_doing { crate::steps::task_started(tx, &r, &a.name())? } else { Vec::new() };
    Ok((r, changed, started))
}

#[tool_router(router = record_tools, vis = "pub(super)")]
impl FlowMcp {
    #[tool(description = "Add a task to a step (or milestone) of your project, with your estimate. For a step's \
        whole plan, or a review's findings, use add_tasks: one call.")]
    async fn add_task(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<AddTask>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let project = target_project(&tx, &a, p.project.as_deref(), true)?;
                let m = projects::find_milestone(&tx, project.id, &p.milestone)?;
                if m.state == "closed" {
                    anyhow::bail!("{} is concluded; add the task to a step that isn't", m.name);
                }
                check_item(&p.task).map_err(|e| anyhow::anyhow!("the task needs {e}"))?;
                let (r, started) = add_one(&tx, project.id, &m, p.task, &a.name())?;
                tx.commit()?;
                let mut reply = brief(&r);
                if !started.is_empty() {
                    reply["started"] = started.into();
                }
                Ok(Outcome::new(reply)
                    .summary(format!("task {} added", r.short_id()))
                    .project(project.slug)
                    .target(r.ref_id))
            })
            .await;
        self.finish(&actor, "add_task", Action::Write, outcome).await
    }

    #[tool(description = "Add several tasks to a step in one call, in the order you'll do them, each with an estimate \
        (and where it came from, once the step is under way). For a review, the findings ranked, with importance; \
        a finding you won't do goes in as skipped, with the reason. All are added or none.")]
    async fn add_tasks(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<AddTasks>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let project = target_project(&tx, &a, p.project.as_deref(), true)?;
                let m = projects::find_milestone(&tx, project.id, &p.milestone)?;
                if m.state == "closed" {
                    anyhow::bail!("{} is concluded; add the tasks to a step that isn't", m.name);
                }
                if p.tasks.is_empty() || p.tasks.len() > 100 {
                    anyhow::bail!("give 1 to 100 tasks");
                }
                let problems: Vec<String> = p
                    .tasks
                    .iter()
                    .enumerate()
                    .filter_map(|(i, t)| check_item(t).err().map(|e| format!("#{}: needs {e}", i + 1)))
                    .collect();
                if !problems.is_empty() {
                    anyhow::bail!("nothing added; {}", problems.join("; "));
                }
                let mut ids = Vec::new();
                let mut started_all = Vec::new();
                // The step is under way once one of them starts; read it fresh for each.
                for t in p.tasks {
                    let m = projects::get_milestone(&tx, m.id)?;
                    let (r, started) = add_one(&tx, project.id, &m, t, &a.name())?;
                    ids.push(r.short_id());
                    started_all.extend(started);
                }
                tx.commit()?;
                let n = ids.len();
                let mut reply = json!({"ids": ids, "milestone": m.name});
                if !started_all.is_empty() {
                    reply["started"] = started_all.into();
                }
                Ok(Outcome::new(reply).summary(format!("{n} tasks added to {}", m.name)).project(project.slug))
            })
            .await;
        self.finish(&actor, "add_tasks", Action::Write, outcome).await
    }

    #[tool(description = "Put a step's tasks in the order you'll do them; the first open one shows as next. \
        Only you set the order (the owner asks you in chat if they want it changed).")]
    async fn order_tasks(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<OrderTasks>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let project = target_project(&tx, &a, p.project.as_deref(), true)?;
                let m = projects::find_milestone(&tx, project.id, &p.milestone)?;
                let tasks = crate::steps::tasks_of(&tx, m.id)?;
                let mut order: Vec<&Record> = Vec::new();
                for id in &p.ids {
                    let r = records::find_str(&tx, id, Some(&project.slug))?;
                    let Some(t) = tasks.iter().find(|t| t.id == r.id) else {
                        anyhow::bail!("{} isn't a task of {}", r.short_id(), m.name);
                    };
                    if !order.iter().any(|o| o.id == t.id) {
                        order.push(t);
                    }
                }
                for t in &tasks {
                    if !order.iter().any(|o| o.id == t.id) {
                        order.push(t);
                    }
                }
                let base = tasks.iter().filter_map(|t| t.position).fold(f64::MAX, f64::min);
                let base = if base == f64::MAX { 1.0 } else { base };
                for (i, t) in order.iter().enumerate() {
                    let pos = base + i as f64;
                    if t.position != Some(pos) {
                        records::update(&tx, t, &a.name(), Patch { position: Some(pos), quiet: true, ..Default::default() })?;
                    }
                }
                let ids: Vec<String> = order.iter().map(|t| t.short_id()).collect();
                tx.commit()?;
                Ok(Outcome::new(json!({"milestone": m.name, "order": ids})).summary(format!("tasks of {} ordered", m.name)).project(project.slug))
            })
            .await;
        self.finish(&actor, "order_tasks", Action::Write, outcome).await
    }

    #[tool(description = "Log how long a build or test took (wall, CPU, peak memory), on a task, a step or a PR. \
        Easier: run it through `agentflow time [--on T-12] -- <command>`, which measures and logs it for you. These \
        replace the timings in PR bodies.")]
    async fn log_timing(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<LogTiming>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let project = target_project(c, &a, None, true)?;
                let on = crate::times::resolve_on(c, project.id, &project.slug, p.on.as_deref())?;
                let t = crate::times::add(
                    c,
                    project.id,
                    &on,
                    &crate::times::NewTiming {
                        what: &p.what,
                        place: p.place.as_deref().unwrap_or("here"),
                        wall_ms: (p.wall_s * 1000.0).round() as i64,
                        cpu_ms: p.cpu_s.map(|s| (s * 1000.0).round() as i64),
                        peak_mb: p.peak_mb,
                        exit_code: None,
                        note: &p.note,
                    },
                    &a.name(),
                )?;
                Ok(Outcome::new(json!({"on": t.on, "logged": t.what})).summary(format!("timing on {}", t.on)).project(project.slug))
            })
            .await;
        self.finish(&actor, "log_timing", Action::Write, outcome).await
    }

    #[tool(description = "Change a task, or several at once with ids: its state (todo, doing, review, done, blocked, \
        parked, skipped), title, details, step, tags, estimate, branch, or when a parked task comes back. Doing starts \
        its step; blocked needs blocked_by; skipped a note on why.")]
    async fn update_task(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<UpdateTask>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let mut ids: Vec<String> = p.ids.clone();
                if !p.id.trim().is_empty() {
                    ids.insert(0, p.id.clone());
                }
                if ids.is_empty() {
                    anyhow::bail!("say which task: id, or ids for several");
                }
                if ids.len() > 50 {
                    anyhow::bail!("at most 50 tasks at once");
                }
                let tx = c.transaction()?;
                let mut out = Vec::new();
                let mut started_all = Vec::new();
                let mut any = false;
                for id in &ids {
                    let (r, changed, started) = update_one(&tx, &a, id, &p)?;
                    any |= changed;
                    started_all.extend(started);
                    out.push(r);
                }
                tx.commit()?;
                let first = out[0].clone();
                let mut reply = if out.len() == 1 { brief(&first) } else { json!({"tasks": out.iter().map(brief).collect::<Vec<_>>()}) };
                if !started_all.is_empty() {
                    reply["started"] = started_all.into();
                }
                let what = if out.len() == 1 { format!("task {}", first.short_id()) } else { format!("{} tasks", out.len()) };
                let summary = format!("{what} {}", if any { "updated" } else { "unchanged" });
                Ok(Outcome::new(reply).summary(summary).project(first.project.clone()).target(first.ref_id))
            })
            .await;
        self.finish(&actor, "update_task", Action::Write, outcome).await
    }

    #[tool(description = "Ask the owner a question. One question per call, with the context they need, numbered \
        options and your recommendation when it's a choice. They answer on their phone or the dashboard; the answer is \
        stored verbatim and shows up in hello and get_record. Don't ask in chat as well. Never stop for a question: \
        when the choice is cheap to change, set going_ahead and build your recommendation meanwhile; otherwise keep \
        working on whatever doesn't depend on it (blocks names what does).")]
    async fn ask_question(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<AskQuestion>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let urgent = p.urgent;
        let going_ahead = p.going_ahead;
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let project = target_project(&tx, &a, None, true)?;
                if let Some(n) = p.recommended
                    && (n == 0 || n as usize > p.options.len())
                {
                    anyhow::bail!("recommended must be one of the options, counting from 1");
                }
                let mut data = Map::new();
                let options: Vec<Value> =
                    p.options.iter().map(|o| json!({"title": o.title.trim(), "detail": o.detail.trim()})).collect();
                if !options.is_empty() {
                    data.insert("options".into(), options.into());
                }
                if let Some(n) = p.recommended {
                    data.insert("recommended".into(), n.into());
                }
                if urgent {
                    data.insert("urgent".into(), true.into());
                }
                if going_ahead {
                    if p.recommended.is_none() {
                        anyhow::bail!("going_ahead needs the option you go ahead with as recommended");
                    }
                    if urgent {
                        anyhow::bail!("a question you go ahead on isn't urgent");
                    }
                    data.insert("going_ahead".into(), true.into());
                }
                let q = records::create(
                    &tx,
                    project.id,
                    &a.name(),
                    NewRecord {
                        kind: Some(Kind::Question),
                        title: p.title,
                        body: p.body,
                        tags: p.tags,
                        data,
                        ..Default::default()
                    },
                )?;
                for b in &p.blocks {
                    let target = records::find_str(&tx, b, Some(&project.slug))?;
                    // A task it blocks waits for the answer (the owner, 2026-11-12: "if its facing
                    // me, it always links to a question"); other records just get the link.
                    if target.kind == Kind::Task && !matches!(target.state.as_str(), "done" | "parked") {
                        crate::steps::block(&tx, &target, &q.ref_id, &a.name())?;
                    } else {
                        records::link(&tx, &q, &target, "blocks", &a.name())?;
                    }
                }
                tx.commit()?;
                Ok((
                    Outcome::new(json!({"id": q.ref_id, "state": "open", "going_ahead": going_ahead}))
                        .summary(format!("question {} asked", q.short_id()))
                        .project(project.slug.clone())
                        .target(q.ref_id.clone()),
                    q,
                    project.name,
                ))
            })
            .await;
        let outcome = match outcome {
            Ok((o, q, name)) => {
                let kind = if urgent { push::Kind::Urgent } else { push::Kind::Question };
                // Going ahead: no notification (the owner, 2026-11-11: no badge, no push).
                if !going_ahead {
                    self.state.push.spawn(
                        kind,
                        Notification {
                            title: format!("{name} asks"),
                            body: q.title.clone(),
                            url: q.url(),
                            tag: Some(q.ref_id.clone()),
                        },
                    );
                }
                Ok(o)
            }
            Err(e) => Err(e),
        };
        self.finish(&actor, "ask_question", Action::Write, outcome).await
    }

    #[tool(description = "Add something the owner needs to test, with a category of your choosing. It stays \
        on your project's To test list, and comes along with every release and build, until the owner tested it: \
        they tick it on a page (you hear it as news) or tell you, and you mark it with resolve_tests.")]
    async fn add_to_test(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<AddToTest>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let project = target_project(c, &a, None, true)?;
                let r = crate::totest::add(c, project.id, &a.name(), &p.title, &p.how, &p.category)?;
                Ok(Outcome::new(json!({"id": r.ref_id, "category": r.str("category"), "open": crate::totest::open_count(c, project.id)?}))
                    .summary(format!("to test {}: {}", r.short_id(), r.title))
                    .project(project.slug)
                    .target(r.ref_id))
            })
            .await;
        self.finish(&actor, "add_to_test", Action::Write, outcome).await
    }

    #[tool(description = "After the owner told you how testing went: mark items works (done), problem (stays \
        open with the problem noted, in their words) or dropped (no longer needed). Items they ticked on a page \
        are done already; you hear those as news.")]
    async fn resolve_tests(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ResolveTests>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                if p.ids.is_empty() {
                    anyhow::bail!("name the items, e.g. [\"TT-3\"]");
                }
                let tx = c.transaction()?;
                let mut done = Vec::new();
                for id in &p.ids {
                    let item = writable(&tx, &a, id)?;
                    let r = crate::totest::agent_resolve(&tx, &item, p.result.trim(), &p.note, &a.name())?;
                    done.push(json!({"id": r.ref_id, "state": r.state}));
                }
                tx.commit()?;
                let n = done.len();
                Ok(Outcome::new(json!({"items": done})).summary(format!("{n} to test: {}", p.result.trim())))
            })
            .await;
        self.finish(&actor, "resolve_tests", Action::Write, outcome).await
    }

    #[tool(description = "Withdraw one of your open questions that no longer needs an answer.")]
    async fn withdraw_question(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<Withdraw>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let q = writable(&tx, &a, &p.id)?;
                if q.kind != Kind::Question || q.state != "open" {
                    anyhow::bail!("{} isn't an open question", q.ref_id);
                }
                if p.reason.trim().is_empty() {
                    anyhow::bail!("say why it's withdrawn");
                }
                let (q, _) = records::update(
                    &tx,
                    &q,
                    &a.name(),
                    Patch { state: Some("withdrawn".into()), note: p.reason, ..Default::default() },
                )?;
                tx.commit()?;
                Ok(Outcome::new(brief(&q))
                    .summary(format!("question {} withdrawn", q.short_id()))
                    .project(q.project.clone())
                    .target(q.ref_id))
            })
            .await;
        self.finish(&actor, "withdraw_question", Action::Write, outcome).await
    }

    #[tool(description = "Record a decision the owner made, with their words verbatim and where they came from. \
        Link the question it answers and the decision it replaces.")]
    async fn record_decision(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<RecordDecision>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let project = target_project(&tx, &a, p.project.as_deref(), true)?;
                if p.owner_words.trim().is_empty() {
                    anyhow::bail!("owner_words: quote what the owner said");
                }
                if p.source.trim().is_empty() {
                    anyhow::bail!("source: \"owner\" or \"relayed by <session>\"");
                }
                let said_at = match &p.said_at {
                    Some(s) => parse_date(s)?,
                    None => now_ms(),
                };
                let mut data = Map::new();
                data.insert("owner_words".into(), p.owner_words.trim().into());
                data.insert("said_at".into(), said_at.into());
                data.insert("source".into(), p.source.trim().into());
                let d = records::create(
                    &tx,
                    project.id,
                    &a.name(),
                    NewRecord {
                        kind: Some(Kind::Decision),
                        title: p.title,
                        body: p.body,
                        tags: p.tags,
                        data,
                        ..Default::default()
                    },
                )?;
                if let Some(q) = &p.answers {
                    let q = records::find_str(&tx, q, Some(&project.slug))?;
                    records::link(&tx, &d, &q, "answers", &a.name())?;
                }
                if let Some(old) = &p.supersedes {
                    let old = records::find_str(&tx, old, Some(&project.slug))?;
                    if old.kind != Kind::Decision {
                        anyhow::bail!("{} isn't a decision", old.ref_id);
                    }
                    records::link(&tx, &d, &old, "supersedes", &a.name())?;
                }
                tx.commit()?;
                Ok(Outcome::new(json!({"id": d.ref_id}))
                    .summary(format!("decision {} recorded", d.short_id()))
                    .project(project.slug)
                    .target(d.ref_id))
            })
            .await;
        self.finish(&actor, "record_decision", Action::Write, outcome).await
    }

    #[tool(description = "Point to an ADR file in a repository (AgentFlow keeps a link, the ADR stays in the repo).")]
    async fn add_adr(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<AddAdr>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let project = target_project(&tx, &a, p.project.as_deref(), true)?;
                let mut data = Map::new();
                data.insert("repo".into(), p.repo.trim().into());
                data.insert("path".into(), p.path.trim().trim_start_matches('/').into());
                if let Some(n) = p.number {
                    data.insert("adr_number".into(), n.into());
                }
                let r = records::create(
                    &tx,
                    project.id,
                    &a.name(),
                    NewRecord { kind: Some(Kind::Adr), title: p.title, tags: p.tags, data, ..Default::default() },
                )?;
                if let Some(d) = &p.records {
                    let d = records::find_str(&tx, d, Some(&project.slug))?;
                    records::link(&tx, &r, &d, "records", &a.name())?;
                }
                tx.commit()?;
                Ok(Outcome::new(json!({"id": r.ref_id}))
                    .summary(format!("ADR {} added", r.short_id()))
                    .project(project.slug)
                    .target(r.ref_id))
            })
            .await;
        self.finish(&actor, "add_adr", Action::Write, outcome).await
    }

    #[tool(
        description = "Link two records: supersedes, answers, blocks, implements (PR → task), records (ADR → decision), \
        depends_on (flags it as possibly stale when the other changes), feedback_on, relates. The first record must be your project's."
    )]
    async fn link(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<Link>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let from = writable(&tx, &a, &p.from)?;
                let to = records::find_str(&tx, &p.to, Some(&from.project))?;
                let added = records::link(&tx, &from, &to, p.relation.trim(), &a.name())?;
                tx.commit()?;
                Ok(Outcome::new(json!({"linked": added}))
                    .summary(format!("{} {} {}", from.ref_id, p.relation.trim(), to.ref_id))
                    .project(from.project.clone())
                    .target(from.ref_id))
            })
            .await;
        self.finish(&actor, "link", Action::Write, outcome).await
    }

    #[tool(description = "One record in full: fields, the owner's words or answer, links, and its history.")]
    async fn get_record(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<GetRecord>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let r = records::find_str(c, &p.id, a.project())?;
                let links: Vec<Value> = records::links_of(c, &r)?
                    .iter()
                    .map(|l| {
                        let mut v = json!({"relation": l.label, "record": l.other.ref_id, "title": l.other.title, "state": l.other.state});
                        if l.stale {
                            v["may_be_stale"] = true.into();
                        }
                        v
                    })
                    .collect();
                let history: Vec<Value> = records::revisions(c, r.id)?
                    .iter()
                    .map(|rv| json!({"rev": rv.rev, "at": fmt_utc(rv.at), "by": rv.author, "changes": rv.changes, "note": rv.note}))
                    .collect();
                let mut reply = serde_json::to_value(&r)?;
                reply["created_at"] = fmt_utc(r.created_at).into();
                reply["updated_at"] = fmt_utc(r.updated_at).into();
                reply["links"] = links.into();
                reply["history"] = history.into();
                Ok(Outcome::new(reply).summary(format!("get_record {}", r.ref_id)).project(r.project.clone()).target(r.ref_id))
            })
            .await;
        self.finish(&actor, "get_record", Action::Read, outcome).await
    }

    #[tool(
        description = "Find records: by project (or \"all\"), kind, state, milestone, tag, words, or changed since a date. \
        Returns ids, titles and states, newest change first; use get_record for one in full."
    )]
    async fn find_records(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<FindRecords>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let all = p.project.as_deref() == Some("all");
                let project = if all { None } else { Some(target_project(c, &a, p.project.as_deref(), false)?) };
                let mut q = Query {
                    project_id: project.as_ref().map(|p| p.id),
                    tag: p.tag,
                    text: p.text,
                    limit: p.limit.unwrap_or(50),
                    ..Default::default()
                };
                if let Some(k) = &p.kind {
                    q.kinds.push(Kind::parse(k)?);
                }
                if let Some(s) = p.state {
                    q.states.push(s);
                }
                if let Some(m) = &p.milestone {
                    let project = project.as_ref().ok_or_else(|| anyhow::anyhow!("milestone needs a project"))?;
                    let m = projects::find_milestone(c, project.id, m)?;
                    q.milestone_ids.push(m.id);
                    q.milestone_ids.extend(
                        projects::milestones(c, project.id)?.iter().filter(|s| s.parent_id == Some(m.id)).map(|s| s.id),
                    );
                }
                if let Some(s) = &p.since {
                    q.since = Some(parse_date(s)?);
                }
                let found = records::query(c, &q)?;
                let list: Vec<Value> = found
                    .iter()
                    .map(|r| {
                        let mut v = brief(r);
                        v["kind"] = r.kind.as_str().into();
                        v["updated"] = fmt_utc(r.updated_at).into();
                        if r.kind == Kind::Decision {
                            v["owner_words"] = r.data.get("owner_words").cloned().unwrap_or(Value::Null);
                        }
                        if r.kind == Kind::Question && r.state == "answered" {
                            v["answer"] = r.data.get("answer").cloned().unwrap_or(Value::Null);
                        }
                        v
                    })
                    .collect();
                let mut o = Outcome::new(json!({"records": list, "count": found.len()}))
                    .summary(format!("find_records: {} found", found.len()));
                if let Some(p) = project {
                    o = o.project(p.slug);
                }
                Ok(o)
            })
            .await;
        self.finish(&actor, "find_records", Action::Read, outcome).await
    }

    #[tool(description = "Send the owner an urgent notification about something that can't wait for them to look: \
        a broken release, a full disk, a blocked stack. Use it rarely; questions go through ask_question.")]
    async fn notify_owner(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<NotifyOwner>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let project = target_project(c, &a, None, true)?;
                if p.title.trim().is_empty() {
                    anyhow::bail!("say what needs the owner");
                }
                let url = match &p.about {
                    Some(id) => records::find_str(c, id, Some(&project.slug))?.url(),
                    None => format!("/p/{}", project.slug),
                };
                records::event(
                    c,
                    Some(project.id),
                    None,
                    "owner.notified",
                    &format!("Urgent: {}", p.title.trim()),
                    &a.name(),
                )?;
                let n = Notification {
                    title: format!("{}: {}", project.name, p.title.trim()),
                    body: p.body.trim().into(),
                    url,
                    tag: None,
                };
                Ok((
                    Outcome::new(json!({"sent": true}))
                        .summary(format!("urgent call: {}", p.title.trim()))
                        .project(project.slug),
                    n,
                ))
            })
            .await;
        let outcome = outcome.map(|(o, n)| {
            self.state.push.spawn(push::Kind::Urgent, n);
            o
        });
        self.finish(&actor, "notify_owner", Action::Push, outcome).await
    }
}
