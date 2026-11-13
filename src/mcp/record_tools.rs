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
    /// The step (or milestone) it belongs to, e.g. "P9.3d" or "M4". Only a parked task may have
    /// none, for things not planned anywhere yet.
    pub milestone: Option<String>,
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
    /// todo (default), doing, parked, skipped (a finding you won't do; needs reason), or done (work
    /// finished before, brought in as history; no estimate needed).
    pub state: Option<String>,
    /// Why it's skipped.
    pub reason: Option<String>,
    /// For a done task brought in as history (e.g. a step folded into another): when it started,
    /// "2026-11-09" or "2026-11-09 14:30" (UTC).
    pub started_at: Option<String>,
    /// For a done task brought in as history: when it was done, like started_at.
    pub done_at: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddTasks {
    /// Only for the consultant: which project.
    pub project: Option<String>,
    /// The step (or milestone) they belong to, e.g. "P9.3d". Left out only when all are parked.
    pub milestone: Option<String>,
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
    /// todo, doing, review, done, blocked, parked, skipped or archived. Set doing when you start it:
    /// that also starts its step. "blocked" needs blocked_by, "skipped" and "archived" a note on why.
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
pub struct AddNote {
    /// What it is, in a few words, e.g. "Loro depends on the unmaintained im crate".
    pub name: String,
    /// A category of your choosing, e.g. "known issue", "technical note", "morning report",
    /// "estimate". Keep using the same ones. For files, use the `flow note <file>` command.
    pub category: String,
    /// The note itself, Markdown.
    pub text: String,
    /// What it belongs to: a record ("T-31") or a step or milestone ("P9.R").
    pub record: Option<String>,
    /// When it should come back to you in hello: a date ("2026-11-25") or a condition ("when Loro
    /// updates", listed in every hello until you resolve it).
    pub resurface: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateNote {
    /// The note, e.g. "N-4".
    pub id: String,
    pub name: Option<String>,
    pub category: Option<String>,
    /// A text note's new text (a file gets a new version with `flow note --replaces N-4`).
    pub text: Option<String>,
    /// When it comes back; empty stops it coming back.
    pub resurface: Option<String>,
    /// "resolved" when a known issue is gone or the note no longer holds (say why in note), or
    /// "current" again.
    pub state: Option<String>,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateDecision {
    /// The decision, e.g. "D-112".
    #[serde(default)]
    pub id: String,
    /// Several at once, e.g. to re-scope many: the same change for each. All or none.
    #[serde(default)]
    pub ids: Vec<String>,
    /// Move it to another scope: a milestone ("M9") or "project" (promote a milestone's decision
    /// to a project-wide one).
    pub scope: Option<String>,
    /// The newer decision that replaces it, e.g. "D-40": linked, and this one marked replaced.
    pub replaced_by: Option<String>,
    pub title: Option<String>,
    /// Why, and what it means in practice.
    pub body: Option<String>,
    /// The owner's words, verbatim: only to correct a quote that was recorded wrong.
    pub owner_words: Option<String>,
    /// When they said it, e.g. "2026-11-06 14:12" (UTC).
    pub said_at: Option<String>,
    pub tags: Option<Vec<String>>,
    /// "archived" puts it away (a duplicate, a mistake, no longer worth keeping); "current" brings
    /// an archived one back. Nothing is deleted: it stays findable, with its history.
    pub state: Option<String>,
    /// Why, in a line. Kept in the decision's history.
    pub reason: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AnswerQuestion {
    /// The question, e.g. "Q-12".
    pub id: String,
    /// What the owner said in chat, verbatim.
    pub owner_words: String,
    /// The option they picked, counting from 1, if they picked one ("lets go with 1").
    pub option: Option<u32>,
    /// The decision it makes, in one statement, e.g. "The sidebar is a list of names"; made from
    /// the question and the answer if left out.
    pub decision: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateScreenshot {
    /// The screenshot, e.g. "S-4".
    pub id: String,
    /// A mockup's state: proposed, approved or built (set built when the PR that implements it
    /// merges).
    pub state: Option<String>,
    /// The design it shows, e.g. the claude.ai artifact link.
    pub design_url: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
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
    /// to todo by themselves when the owner answers. A PR or release (e.g. "R-1", whose tag name is
    /// the question) keeps its buttons locked until then.
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
    /// The owner is talking with you right now and you're asking it in that chat too: it's
    /// recorded, but sends no notification and isn't counted in the badge (the owner,
    /// 2026-11-12: "when I am just talking to it, I don't want to always get notifications").
    #[serde(default)]
    pub in_chat: bool,
    /// Where its answer applies (it becomes a decision there): a milestone ("M9"; a step means its
    /// milestone) or "project". Default: the current milestone. The owner can change it on the page.
    pub scope: Option<String>,
    /// Earlier decisions this question touches, e.g. ["D-12"]: shown with it, so the owner sees
    /// what they might change. What the answer means for them is yours to act on afterwards.
    #[serde(default)]
    pub related: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateQuestion {
    /// The question, e.g. "Q-4".
    #[serde(default)]
    pub id: String,
    /// Several at once: the same change for each. All or none.
    #[serde(default)]
    pub ids: Vec<String>,
    /// Move it to another scope: a milestone ("M9") or "project".
    pub scope: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    /// "archived" puts it away (no longer needed; it stops blocking its tasks and leaves the owner's
    /// list); "open" or "answered" brings an archived one back as it was.
    pub state: Option<String>,
    /// Why, in a line. Kept in its history.
    pub reason: String,
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
    /// Where it applies: a milestone ("M9"; a step means its milestone) or "project" for what
    /// holds across the project (rules about the owner, the architecture, the process). Default:
    /// the question's scope when it answers one, otherwise the current milestone.
    pub scope: Option<String>,
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
    /// Decisions and questions are those in force by default: project-wide ones and those of
    /// milestones that aren't closed, without replaced decisions. include_closed adds closed
    /// milestones' ones and replaced decisions.
    #[serde(default)]
    pub include_closed: bool,
    /// Archived records too (milestones' and steps' tasks, decisions, questions).
    #[serde(default)]
    pub include_archived: bool,
    /// Skip this many, for the next page (see total and more in the reply).
    pub offset: Option<i64>,
    /// Only id, title, state and scope (or milestone): for long lists such as every decision.
    #[serde(default)]
    pub compact: bool,
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
    if matches!(r.kind, Kind::Decision | Kind::Question) {
        v["scope"] = r.milestone.clone().unwrap_or_else(|| "project".into()).into();
    } else if let Some(m) = &r.milestone {
        v["milestone"] = m.clone().into();
    }
    v
}

fn parse_date(s: &str) -> anyhow::Result<i64> {
    crate::util::parse_day_or_time(s)
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
    if !["todo", "doing", "parked", "skipped", "done"].contains(&state) {
        return Err(format!("state {state:?}: add tasks as todo, doing, parked, skipped or done (block them afterwards with update_task)"));
    }
    if t.title.trim().is_empty() {
        return Err("a title".into());
    }
    if state == "skipped" {
        if t.reason.as_deref().map(str::trim).unwrap_or("").is_empty() {
            return Err("a reason it's skipped".into());
        }
    } else if state == "done" && t.estimate.as_deref().map(str::trim).unwrap_or("").is_empty() {
        // History: done before, no estimate to set against.
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
    for (k, v) in [("started_at", &t.started_at), ("done_at", &t.done_at)] {
        if let Some(v) = v.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            if t.state.as_deref() != Some("done") {
                return Err(format!("{k} is for a done task brought in as history"));
            }
            crate::util::parse_day_or_time(v).map_err(|e| format!("{k}: {e:#}"))?;
        }
    }
    Ok(())
}

/// Adds one checked task to a step. Added while the step is under way (and not said to be
/// planned), it's marked added, with where it came from ("found" unless said).
fn add_one(tx: &rusqlite::Connection, project_id: i64, m: Option<&projects::Milestone>, t: TaskItem, actor: &str) -> anyhow::Result<(Record, Vec<String>)> {
    let state = t.state.clone().unwrap_or_else(|| "todo".into());
    if m.is_none() && state != "parked" {
        anyhow::bail!("only a parked task can be without a step or milestone");
    }
    let under_way = m.is_some_and(|m| m.state == "active") && state != "done";
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
    for (k, v) in [("started_at", &t.started_at), ("done_at", &t.done_at)] {
        if let Some(v) = v.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            data.insert(k.into(), crate::util::parse_day_or_time(v)?.into());
        }
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
            milestone_id: m.map(|m| m.id),
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
    let mut r = writable(tx, a, id)?;
    if r.kind != Kind::Task {
        anyhow::bail!("{} isn't a task", r.ref_id);
    }
    // Archived: hidden, kept, restorable (the owner, 2026-11-12: agents archive tasks freely).
    if p.state.as_deref() == Some("archived") {
        if p.note.trim().is_empty() {
            anyhow::bail!("say why it's archived, in note");
        }
        let changed = r.state != "archived";
        let r = crate::steps::archive_record(tx, &r, &p.note, None, &a.name())?;
        return Ok((r, changed, Vec::new()));
    }
    if r.state == "archived" && p.state.is_some() {
        r = crate::steps::restore_record(tx, &r, &a.name())?;
    }
    let milestone_id = match &p.milestone {
        Some(name) => {
            // Any step, concluded ones too (the owner, 2026-11-12: agents edit steps and tasks freely).
            let m = projects::find_milestone(tx, r.project_id, name)?;
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

/// `id` or `ids`, not both, at least one.
fn one_or_many(id: &str, ids: &[String]) -> anyhow::Result<Vec<String>> {
    let id = id.trim();
    match (id.is_empty(), ids.is_empty()) {
        (false, true) => Ok(vec![id.to_owned()]),
        (true, false) => Ok(ids.iter().map(|i| i.trim().to_owned()).collect()),
        (true, true) => anyhow::bail!("give id, or ids for several"),
        (false, false) => anyhow::bail!("give id or ids, not both"),
    }
}

fn update_decision_one(tx: &rusqlite::Connection, a: &crate::auth::Actor, id: &str, p: &UpdateDecision) -> anyhow::Result<(Record, bool)> {
    let d = writable(tx, a, id)?;
    if d.kind != Kind::Decision {
        anyhow::bail!("{} isn't a decision", d.ref_id);
    }
    let reason = p.reason.trim();
    if reason.is_empty() {
        anyhow::bail!("reason: say why, in a line");
    }
    if let Some(st) = &p.state
        && !["archived", "current"].contains(&st.as_str())
    {
        anyhow::bail!("state: archived, or current to bring an archived one back (replaced_by marks one replaced)");
    }
    if p.state.as_deref() == Some("current") && d.state != "archived" {
        anyhow::bail!("{} is {}, not archived", d.short_id(), d.state);
    }
    let mut data = Map::new();
    if let Some(w) = p.owner_words.as_deref().map(str::trim) {
        if w.is_empty() {
            anyhow::bail!("owner_words can't be empty; archive the decision instead");
        }
        data.insert("owner_words".into(), w.into());
    }
    if let Some(at) = &p.said_at {
        data.insert("said_at".into(), parse_date(at)?.into());
    }
    let milestone_id = match p.scope.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Some(projects::scope(tx, d.project_id, Some(s))?.map(|m| m.id)),
        None => None,
    };
    let (mut d, mut changed) = records::update(
        tx,
        &d,
        &a.name(),
        Patch { title: p.title.clone(), body: p.body.clone(), tags: p.tags.clone(), milestone_id, data, note: reason.to_owned(), ..Default::default() },
    )?;
    if let Some(by) = p.replaced_by.as_deref().map(str::trim).filter(|b| !b.is_empty()) {
        let newer = records::find_str(tx, by, Some(&d.project))?;
        if newer.kind != Kind::Decision || newer.project_id != d.project_id {
            anyhow::bail!("replaced_by is a newer decision of the same project");
        }
        changed |= records::link(tx, &newer, &d, "supersedes", &a.name())?;
        d = records::get(tx, d.id)?;
    }
    match p.state.as_deref() {
        Some("archived") if d.state != "archived" => {
            d = crate::steps::archive_record(tx, &d, reason, None, &a.name())?;
            changed = true;
        }
        Some("current") => {
            d = crate::steps::restore_record(tx, &d, &a.name())?;
            changed = true;
        }
        _ => {}
    }
    Ok((d, changed))
}

/// Records one decision (record_decision, and each of record_decisions). `earlier` maps "#n" to
/// the decisions made before it in the same batch, so a batch can replace its own entries.
fn decide(tx: &rusqlite::Connection, a: &crate::auth::Actor, project: &projects::Project, p: &RecordDecision, earlier: &[Record]) -> anyhow::Result<Record> {
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
    let find = |id: &str| -> anyhow::Result<Record> {
        if let Some(n) = id.trim().strip_prefix('#').and_then(|n| n.parse::<usize>().ok()) {
            return earlier.get(n.wrapping_sub(1)).cloned().ok_or_else(|| anyhow::anyhow!("#{n} isn't an earlier decision of this batch"));
        }
        records::find_str(tx, id, Some(&project.slug))
    };
    let mut data = Map::new();
    data.insert("owner_words".into(), p.owner_words.trim().into());
    data.insert("said_at".into(), said_at.into());
    data.insert("source".into(), p.source.trim().into());
    // The question's scope when it answers one and none is given.
    let scope = match (&p.scope, &p.answers) {
        (None, Some(q)) => match find(q) {
            Ok(q) if q.kind == Kind::Question => q.milestone_id,
            _ => projects::scope(tx, project.id, None)?.map(|m| m.id),
        },
        _ => projects::scope(tx, project.id, p.scope.as_deref())?.map(|m| m.id),
    };
    let d = records::create(
        tx,
        project.id,
        &a.name(),
        NewRecord {
            kind: Some(Kind::Decision),
            title: p.title.clone(),
            body: p.body.clone(),
            tags: p.tags.clone(),
            milestone_id: scope,
            data,
            ..Default::default()
        },
    )?;
    if let Some(q) = &p.answers {
        let q = find(q)?;
        // An answer given in chat closes the question too (the owner, 2026-11-12).
        if q.kind == Kind::Question && q.state == "open" {
            records::answer_question(tx, &q, None, p.owner_words.trim(), &a.name())?;
        }
        records::link(tx, &d, &q, "answers", &a.name())?;
    }
    if let Some(old) = &p.supersedes {
        let old = find(old)?;
        if old.kind != Kind::Decision {
            anyhow::bail!("{} isn't a decision", old.ref_id);
        }
        records::link(tx, &d, &old, "supersedes", &a.name())?;
    }
    Ok(d)
}

fn adr(tx: &rusqlite::Connection, a: &crate::auth::Actor, project: &projects::Project, p: &AddAdr) -> anyhow::Result<Record> {
    let mut data = Map::new();
    data.insert("repo".into(), p.repo.trim().into());
    data.insert("path".into(), p.path.trim().trim_start_matches('/').into());
    if let Some(n) = p.number {
        data.insert("adr_number".into(), n.into());
    }
    let r = records::create(
        tx,
        project.id,
        &a.name(),
        NewRecord { kind: Some(Kind::Adr), title: p.title.clone(), tags: p.tags.clone(), data, ..Default::default() },
    )?;
    if let Some(d) = &p.records {
        let d = records::find_str(tx, d, Some(&project.slug))?;
        records::link(tx, &r, &d, "records", &a.name())?;
    }
    Ok(r)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecordDecisions {
    /// Only for the consultant: which project.
    pub project: Option<String>,
    /// The decisions, oldest first. In supersedes or answers, "#3" means the 3rd decision of this
    /// list. All are recorded or none (with each problem listed).
    pub decisions: Vec<RecordDecision>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddAdrs {
    /// Only for the consultant: which project.
    pub project: Option<String>,
    pub adrs: Vec<AddAdr>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddToTests {
    pub items: Vec<AddToTest>,
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
                let m = match p.milestone.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
                    Some(name) => Some(projects::find_milestone(&tx, project.id, name)?),
                    None => None,
                };
                let history = p.task.state.as_deref() == Some("done");
                if let Some(m) = &m
                    && m.state == "closed"
                    && !history
                {
                    anyhow::bail!("{} is concluded; add the task to a step that isn't", m.name);
                }
                check_item(&p.task).map_err(|e| anyhow::anyhow!("the task needs {e}"))?;
                let (r, started) = add_one(&tx, project.id, m.as_ref(), p.task, &a.name())?;
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
        (and where it came from, once the step is under way). Done tasks brought in as history need no estimate and \
        take started_at and done_at. For a review, the findings ranked, with importance; \
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
                let m = match p.milestone.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
                    Some(name) => Some(projects::find_milestone(&tx, project.id, name)?),
                    None => None,
                };
                let history = p.tasks.iter().all(|t| t.state.as_deref() == Some("done"));
                if let Some(m) = &m
                    && m.state == "closed"
                    && !history
                {
                    anyhow::bail!("{} is concluded; add the tasks to a step that isn't (done tasks, as history, may go there)", m.name);
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
                    let m = match &m {
                        Some(m) => Some(projects::get_milestone(&tx, m.id)?),
                        None => None,
                    };
                    let (r, started) = add_one(&tx, project.id, m.as_ref(), t, &a.name())?;
                    ids.push(r.short_id());
                    started_all.extend(started);
                }
                tx.commit()?;
                let n = ids.len();
                let name = m.as_ref().map(|m| m.name.clone()).unwrap_or_default();
                let mut reply = json!({"ids": ids, "milestone": name});
                if !started_all.is_empty() {
                    reply["started"] = started_all.into();
                }
                Ok(Outcome::new(reply).summary(format!("{n} tasks added to {}", if name.is_empty() { "no milestone" } else { name.as_str() })).project(project.slug))
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
        Easier: run it through `flow time [--on T-12] -- <command>`, which measures and logs it for you. These \
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

    #[tool(description = "Write a short note: a known issue, a technical note, an estimate, anything worth keeping \
        that isn't a task or a decision. Files (reports, plans, benchmarks, logs) go up with `flow note <file> \
        --name … --category …`. A note can come back to you in hello: on a date, or until you resolve it.")]
    async fn add_note(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<AddNote>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let project = target_project(&tx, &a, None, true)?;
                let category = crate::screenshots::category(&p.category)?;
                if p.text.trim().is_empty() {
                    anyhow::bail!("a text note needs its text; a file goes up with `flow note <file>`");
                }
                if p.text.len() > 64 * 1024 {
                    anyhow::bail!("that's long for a text note; upload it as a file with `flow note <file>`");
                }
                let mut data = Map::new();
                data.insert("category".into(), category.into());
                if let Some(r) = p.resurface.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
                    let (at, text) = crate::notes::resurface(r)?;
                    data.insert("resurface".into(), text.into());
                    data.insert("resurface_at".into(), at.map(Value::from).unwrap_or(Value::Null));
                }
                let (target, step) = crate::notes::target(&tx, &project, p.record.as_deref().unwrap_or("").trim())?;
                let n = records::create(
                    &tx,
                    project.id,
                    &a.name(),
                    NewRecord { kind: Some(Kind::Note), title: p.name, body: p.text, milestone_id: step, data, ..Default::default() },
                )?;
                if let Some(target) = &target {
                    records::link(&tx, &n, target, "relates", &a.name())?;
                }
                tx.commit()?;
                Ok(Outcome::new(json!({"id": n.ref_id})).summary(format!("note {} added", n.short_id())).project(project.slug).target(n.ref_id))
            })
            .await;
        self.finish(&actor, "add_note", Action::Write, outcome).await
    }

    #[tool(description = "Change a note: its name, category, text (text notes), when it comes back, or resolve it \
        (a known issue that's gone, a note that no longer holds; say why).")]
    async fn update_note(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<UpdateNote>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let n = writable(&tx, &a, &p.id)?;
                if n.kind != Kind::Note {
                    anyhow::bail!("{} isn't a note", n.ref_id);
                }
                if p.text.is_some() && !n.str("ext").is_empty() {
                    anyhow::bail!("{} is a file; upload a new version with `flow note <file> --replaces {}`", n.short_id(), n.short_id());
                }
                if p.state.as_deref() == Some("resolved") && p.note.trim().is_empty() {
                    anyhow::bail!("say why it's resolved, in note");
                }
                let mut data = Map::new();
                if let Some(cat) = &p.category {
                    data.insert("category".into(), crate::screenshots::category(cat)?.into());
                }
                if let Some(r) = &p.resurface {
                    if r.trim().is_empty() {
                        data.insert("resurface".into(), Value::Null);
                        data.insert("resurface_at".into(), Value::Null);
                    } else {
                        let (at, text) = crate::notes::resurface(r)?;
                        data.insert("resurface".into(), text.into());
                        data.insert("resurface_at".into(), at.map(Value::from).unwrap_or(Value::Null));
                    }
                }
                let (n, changed) = records::update(
                    &tx,
                    &n,
                    &a.name(),
                    Patch { title: p.name, body: p.text, state: p.state, data, note: p.note, ..Default::default() },
                )?;
                tx.commit()?;
                Ok(Outcome::new(json!({"id": n.ref_id, "state": n.state, "changed": changed}))
                    .summary(format!("note {} updated", n.short_id()))
                    .project(n.project.clone())
                    .target(n.ref_id))
            })
            .await;
        self.finish(&actor, "update_note", Action::Write, outcome).await
    }

    #[tool(description = "Change a screenshot: a mockup's state (proposed, approved, built), the design link, its \
        name or description. Upload screenshots with `flow screenshot` (for a mockup: --design-url and --state).")]
    async fn update_screenshot(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<UpdateScreenshot>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let r = writable(&tx, &a, &p.id)?;
                if r.kind != Kind::Screenshot {
                    anyhow::bail!("{} isn't a screenshot", r.ref_id);
                }
                let mut data = Map::new();
                if let Some(st) = p.state.as_deref().map(str::trim) {
                    crate::screenshots::check_mockup_state(st)?;
                    data.insert("mockup_state".into(), st.into());
                }
                if let Some(u) = p.design_url.as_deref().map(str::trim) {
                    if u.is_empty() {
                        data.insert("design_url".into(), Value::Null);
                    } else {
                        crate::screenshots::check_design_url(u)?;
                        data.insert("design_url".into(), u.into());
                    }
                }
                let (r, changed) = records::update(&tx, &r, &a.name(), Patch { title: p.name, body: p.description, data, ..Default::default() })?;
                tx.commit()?;
                Ok(Outcome::new(json!({"id": r.ref_id, "mockup_state": r.data.get("mockup_state"), "changed": changed}))
                    .summary(format!("screenshot {} updated", r.short_id()))
                    .project(r.project.clone())
                    .target(r.ref_id))
            })
            .await;
        self.finish(&actor, "update_screenshot", Action::Write, outcome).await
    }

    #[tool(description = "The owner answered one of your questions in chat: record it. Answers the question with their \
        words, unblocks the tasks it blocked, and records the decision in the question's scope (with their words, said \
        now). Then check whether it changes an earlier decision (update_decision). Questions they answer in AgentFlow \
        need nothing from you: the decision is recorded by itself, and you hear of it in your news.")]
    async fn answer_question(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<AnswerQuestion>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let q = writable(&tx, &a, &p.id)?;
                if q.kind != Kind::Question {
                    anyhow::bail!("{} isn't a question", q.ref_id);
                }
                if p.owner_words.trim().is_empty() {
                    anyhow::bail!("owner_words: quote what the owner said");
                }
                let (q, d) = records::answer_and_decide(&tx, &q, p.option, p.owner_words.trim(), &a.name(), p.decision.as_deref(), "owner, in chat")?;
                let scope = projects::scope_name(&tx, d.milestone_id);
                tx.commit()?;
                Ok(Outcome::new(json!({"question": q.ref_id, "state": q.state, "decision": d.ref_id, "scope": scope,
                    "check": "does this change an earlier decision? update_decision: edit, archive, scope or replaced_by"}))
                    .summary(format!("{} answered in chat; decision {}", q.short_id(), d.short_id()))
                    .project(q.project.clone())
                    .target(q.ref_id))
            })
            .await;
        self.finish(&actor, "answer_question", Action::Write, outcome).await
    }

    #[tool(description = "Change decisions, one or several at once with ids (the owner, 2026-11-12: agents edit \
        and archive decisions freely): fix the title, why, tags, a wrongly recorded quote or date; move it to another \
        scope (a milestone, or \"project\" to promote it); mark it replaced by a newer one (replaced_by); archive it \
        (state archived: hidden, kept, restorable with state current). After an answer becomes a decision, check \
        whether it changes an earlier one and do whichever fits. Every change keeps the old text in the history, \
        with your reason. All or none.")]
    async fn update_decision(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<UpdateDecision>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let ids = one_or_many(&p.id, &p.ids)?;
                let mut done = Vec::new();
                let mut project = String::new();
                for id in &ids {
                    let (d, changed) = update_decision_one(&tx, &a, id, &p)?;
                    project = d.project.clone();
                    done.push(json!({"id": d.ref_id, "state": d.state, "scope": projects::scope_name(&tx, d.milestone_id), "changed": changed}));
                }
                tx.commit()?;
                let reply = if done.len() == 1 { done[0].clone() } else { json!({"decisions": done}) };
                Ok(Outcome::new(reply).summary(format!("{} decision(s) updated", ids.len())).project(project))
            })
            .await;
        self.finish(&actor, "update_decision", Action::Write, outcome).await
    }

    #[tool(description = "Change a task, or several at once with ids: its state (todo, doing, review, done, blocked, \
        parked, skipped, archived), title, details, step (any step, concluded ones too), tags, estimate, branch, or when \
        a parked task comes back. Doing starts its step; blocked needs blocked_by; skipped and archived a note on why. \
        Archived tasks are hidden and kept; any other state brings one back.")]
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

    #[tool(description = "Change questions, one or several at once with ids (the owner, 2026-11-12: agents edit \
        and archive questions freely): their scope (where the answer's decision lands), title or details; archive one \
        that's no longer needed (it stops blocking its tasks and leaves the owner's list), or bring an archived one \
        back. All or none, with your reason in the history.")]
    async fn update_question(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<UpdateQuestion>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let ids = one_or_many(&p.id, &p.ids)?;
                let reason = p.reason.trim();
                if reason.is_empty() {
                    anyhow::bail!("reason: say why, in a line");
                }
                let mut done = Vec::new();
                let mut project = String::new();
                for id in &ids {
                    let q = writable(&tx, &a, id)?;
                    if q.kind != Kind::Question {
                        anyhow::bail!("{} isn't a question", q.ref_id);
                    }
                    let milestone_id = match p.scope.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                        Some(s) => Some(projects::scope(&tx, q.project_id, Some(s))?.map(|m| m.id)),
                        None => None,
                    };
                    let (mut q, mut changed) = records::update(
                        &tx,
                        &q,
                        &a.name(),
                        Patch { title: p.title.clone(), body: p.body.clone(), milestone_id, note: reason.to_owned(), ..Default::default() },
                    )?;
                    match p.state.as_deref() {
                        None => {}
                        Some("archived") => {
                            if q.state != "archived" {
                                q = crate::steps::archive_record(&tx, &q, reason, None, &a.name())?;
                                changed = true;
                            }
                        }
                        Some(st @ ("open" | "answered")) => {
                            if q.state != "archived" {
                                anyhow::bail!("{} is {}; only an archived question is brought back", q.short_id(), q.state);
                            }
                            q = crate::steps::restore_record(&tx, &q, &a.name())?;
                            if q.state != st {
                                anyhow::bail!("{} comes back as {} (what it was)", q.short_id(), q.state);
                            }
                            changed = true;
                        }
                        Some(other) => anyhow::bail!("state {other:?}: archived, or open/answered to bring one back"),
                    }
                    project = q.project.clone();
                    done.push(json!({"id": q.ref_id, "state": q.state, "scope": projects::scope_name(&tx, q.milestone_id), "changed": changed}));
                }
                tx.commit()?;
                let reply = if done.len() == 1 { done[0].clone() } else { json!({"questions": done}) };
                Ok(Outcome::new(reply).summary(format!("{} question(s) updated", ids.len())).project(project))
            })
            .await;
        self.finish(&actor, "update_question", Action::Write, outcome).await
    }

    #[tool(description = "Ask the owner a question. One question per call, with the context they need, numbered \
        options and your recommendation when it's a choice. Ask in chat as well: your chat reply (or the summary at the \
        end of a long task) repeats the same numbered questions with their ids (Q-12), so the owner can answer in either \
        place (the owner, 2026-11-12). When the owner is talking with you right now, set in_chat: no notification. \
        Give it a scope (a milestone, default the current one, or \"project\"): the answer becomes a decision there by \
        itself, and you hear of it in your news (one given in chat you record with answer_question). Name earlier \
        decisions it touches in related; afterwards, what the answer means for them is yours to act on. Never stop for a question: when the \
        choice is cheap to change, set going_ahead and build your recommendation meanwhile; otherwise keep working on \
        whatever doesn't depend on it (blocks names the tasks that wait, and they come back when it's answered).")]
    async fn ask_question(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<AskQuestion>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let urgent = p.urgent;
        let going_ahead = p.going_ahead;
        let in_chat = p.in_chat;
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
                if in_chat {
                    if urgent {
                        anyhow::bail!("a question asked in chat isn't urgent: the owner is right there");
                    }
                    data.insert("in_chat".into(), true.into());
                }
                let scope = projects::scope(&tx, project.id, p.scope.as_deref())?;
                let related: Vec<Record> = p
                    .related
                    .iter()
                    .map(|r| {
                        let d = records::find_str(&tx, r, Some(&project.slug))?;
                        if d.kind != Kind::Decision || d.project_id != project.id {
                            anyhow::bail!("related: {} isn't a decision of {}", d.ref_id, project.slug);
                        }
                        Ok(d)
                    })
                    .collect::<anyhow::Result<_>>()?;
                let q = records::create(
                    &tx,
                    project.id,
                    &a.name(),
                    NewRecord {
                        kind: Some(Kind::Question),
                        title: p.title,
                        body: p.body,
                        tags: p.tags,
                        milestone_id: scope.as_ref().map(|m| m.id),
                        data,
                        ..Default::default()
                    },
                )?;
                for d in &related {
                    records::link(&tx, &q, d, "relates", &a.name())?;
                }
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
                    Outcome::new(json!({"id": q.ref_id, "state": "open", "going_ahead": going_ahead,
                        "scope": scope.as_ref().map(|m| m.name.clone()).unwrap_or_else(|| "project".into())}))
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
                // Going ahead or asked in chat: no notification (the owner, 2026-11-11: no badge,
                // no push; 2026-11-12 for chat).
                if !going_ahead && !in_chat {
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
                let d = decide(&tx, &a, &project, &p, &[])?;
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
                let r = adr(&tx, &a, &project, &p)?;
                tx.commit()?;
                Ok(Outcome::new(json!({"id": r.ref_id}))
                    .summary(format!("ADR {} added", r.short_id()))
                    .project(project.slug)
                    .target(r.ref_id))
            })
            .await;
        self.finish(&actor, "add_adr", Action::Write, outcome).await
    }

    #[tool(description = "Record several of the owner's decisions in one call, oldest first, each with their words \
        verbatim (for an import or a long chat). In supersedes or answers, \"#3\" means the 3rd of this list. All are \
        recorded or none.")]
    async fn record_decisions(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<RecordDecisions>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                if p.decisions.is_empty() || p.decisions.len() > 100 {
                    anyhow::bail!("give 1 to 100 decisions");
                }
                let tx = c.transaction()?;
                let project = target_project(&tx, &a, p.project.as_deref(), true)?;
                let mut made: Vec<Record> = Vec::new();
                for (i, d) in p.decisions.iter().enumerate() {
                    let r = decide(&tx, &a, &project, d, &made).map_err(|e| anyhow::anyhow!("nothing recorded; #{}: {e:#}", i + 1))?;
                    made.push(r);
                }
                tx.commit()?;
                let ids: Vec<String> = made.iter().map(|r| r.short_id()).collect();
                Ok(Outcome::new(json!({"ids": ids})).summary(format!("{} decisions recorded", ids.len())).project(project.slug))
            })
            .await;
        self.finish(&actor, "record_decisions", Action::Write, outcome).await
    }

    #[tool(description = "Point to several ADR files in one call (AgentFlow keeps links; the ADRs stay in the repo). \
        All are added or none.")]
    async fn add_adrs(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<AddAdrs>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                if p.adrs.is_empty() || p.adrs.len() > 100 {
                    anyhow::bail!("give 1 to 100 ADRs");
                }
                let tx = c.transaction()?;
                let project = target_project(&tx, &a, p.project.as_deref(), true)?;
                let mut ids = Vec::new();
                for (i, x) in p.adrs.iter().enumerate() {
                    let r = adr(&tx, &a, &project, x).map_err(|e| anyhow::anyhow!("nothing added; #{}: {e:#}", i + 1))?;
                    ids.push(r.short_id());
                }
                tx.commit()?;
                Ok(Outcome::new(json!({"ids": ids})).summary(format!("{} ADRs added", ids.len())).project(project.slug))
            })
            .await;
        self.finish(&actor, "add_adrs", Action::Write, outcome).await
    }

    #[tool(description = "Add several things for the owner to test in one call (each with a category of your \
        choosing). All are added or none.")]
    async fn add_to_tests(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<AddToTests>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                if p.items.is_empty() || p.items.len() > 100 {
                    anyhow::bail!("give 1 to 100 items");
                }
                let tx = c.transaction()?;
                let project = target_project(&tx, &a, None, true)?;
                let mut ids = Vec::new();
                for (i, t) in p.items.iter().enumerate() {
                    let r = crate::totest::add(&tx, project.id, &a.name(), &t.title, &t.how, &t.category)
                        .map_err(|e| anyhow::anyhow!("nothing added; #{}: {e:#}", i + 1))?;
                    ids.push(r.short_id());
                }
                let open = crate::totest::open_count(&tx, project.id)?;
                tx.commit()?;
                Ok(Outcome::new(json!({"ids": ids, "open": open})).summary(format!("{} to-test items added", ids.len())).project(project.slug))
            })
            .await;
        self.finish(&actor, "add_to_tests", Action::Write, outcome).await
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
                    include_archived: p.include_archived,
                    // Asking for a milestone reaches past the default (and a state, for replaced ones).
                    open_scopes: !p.include_closed && p.milestone.is_none(),
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
                    let m = projects::find_milestone_any(c, project.id, m, p.include_archived)?;
                    q.milestone_ids.push(m.id);
                    let mut all = projects::milestones(c, project.id)?;
                    if p.include_archived {
                        all.extend(projects::archived_milestones(c, project.id)?);
                    }
                    q.milestone_ids.extend(all.iter().filter(|s| s.parent_id == Some(m.id)).map(|s| s.id));
                }
                if let Some(s) = &p.since {
                    q.since = Some(parse_date(s)?);
                }
                // Everything that matches, then the page asked for: the reply says how many in all.
                let limit = q.limit.clamp(1, 1000);
                let offset = p.offset.unwrap_or(0).max(0) as usize;
                q.limit = 0;
                let everything = records::query(c, &q)?;
                let total = everything.len();
                let found: Vec<Record> = everything.into_iter().skip(offset).take(limit as usize).collect();
                let more = offset + found.len() < total;
                let list: Vec<Value> = found
                    .iter()
                    .map(|r| {
                        let mut v = brief(r);
                        if p.compact {
                            return v;
                        }
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
                let mut o = Outcome::new(json!({"records": list, "count": found.len(), "total": total, "more": more}))
                    .summary(format!("find_records: {} of {total}", found.len()));
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
