//! Tools about projects, their agent and milestones.

use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{FlowMcp, Outcome, actor_of, target_project};
use crate::activity::Action;
use crate::projects::{self, MilestonePatch, NewMilestone, ProjectPatch};
use crate::records::{self, Kind, Query};
use crate::util::now_ms;

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct Hello {
    /// Your session's name, e.g. "Vinculum".
    pub session: String,
    /// The machine you run on, e.g. "giosuel.ch" or "HEADQUARTERS".
    pub machine: String,
    /// Your model, e.g. "Claude Opus 5.5".
    pub model: String,
    /// Your effort level, e.g. "high".
    pub effort: String,
    /// After your context was reset or summarized in the same session: only where you are, news
    /// and notes coming back, without the project document and milestones.
    #[serde(default)]
    pub brief: bool,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SinceLastCheck {
    /// See news again that was already read: everything from others in this last stretch, e.g.
    /// "1h" or "30 min". The bookmark stays where it is.
    pub again: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectArg {
    /// A project's slug, e.g. "vinculum". Leave out for your own project.
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateProject {
    /// Only for the consultant: which project. Agents change their own project.
    pub project: Option<String>,
    #[serde(flatten)]
    pub patch: ProjectPatch,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WorkingOn {
    /// One line the owner sees on the project page, e.g. "T-14 and T-16, with 2 subagents". Empty clears it.
    pub note: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddMilestone {
    /// Only for the consultant: which project.
    pub project: Option<String>,
    /// Short name: Mn for a main milestone, An for an addendum to Mn after it closed (fixes,
    /// follow-ups, feedback), Pn for preparation before Mn starts; or K1, K2… for steps.
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// Makes this a step of that milestone, e.g. "M4".
    pub step_of: Option<String>,
    /// Insert it before this milestone (name); at the end if left out.
    pub before: Option<String>,
    /// Makes it a review step (the owner, 2026-11-12: a review is a step of its own). Add its
    /// findings with add_tasks, ranked, with importance; skipped ones with a reason.
    pub review: Option<ReviewInfo>,
    /// Its state right away (planned if left out), e.g. "closed" for a concluded step brought in
    /// as history, with started_at, closed_at ("2026-11-09" or "2026-11-09 14:30", UTC) and summary.
    pub state: Option<String>,
    pub started_at: Option<String>,
    pub closed_at: Option<String>,
    pub summary: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReviewInfo {
    /// Who reviews, e.g. ["correctness", "UX", "performance"] (fresh agents, no context of the work).
    #[serde(default)]
    pub reviewers: Vec<String>,
    /// The commit reviewed, e.g. "9f3c2e1" (on main, or say the branch).
    #[serde(default)]
    pub commit: String,
    /// The owner's words for the review, verbatim, if they gave any.
    pub owner_words: Option<String>,
    /// The reports, as notes or links, e.g. ["N-6"].
    pub reports: Option<Vec<String>>,
}

impl ReviewInfo {
    /// This review over an earlier one: what's left out stays as it was.
    fn over(&self, old: &str) -> Value {
        let old: Value = serde_json::from_str(old).unwrap_or(Value::Null);
        let reviewers = if self.reviewers.is_empty() { old.get("reviewers").cloned().unwrap_or_else(|| json!([])) } else { json!(self.reviewers) };
        let commit = match self.commit.trim() {
            "" => old.get("commit").and_then(Value::as_str).unwrap_or("").to_owned(),
            c => c.to_owned(),
        };
        let owner_words = match &self.owner_words {
            Some(w) => w.trim().to_owned(),
            None => old.get("owner_words").and_then(Value::as_str).unwrap_or("").to_owned(),
        };
        let reports = match &self.reports {
            Some(r) => json!(r),
            None => old.get("reports").cloned().unwrap_or_else(|| json!([])),
        };
        json!({"reviewers": reviewers, "commit": commit, "owner_words": owner_words, "reports": reports})
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateMilestone {
    /// Only for the consultant: which project.
    pub project: Option<String>,
    /// The milestone or step, e.g. "M4", or "M4/K2" if the step name repeats.
    pub milestone: String,
    #[serde(flatten)]
    pub patch: MilestonePatch,
    /// Makes it a review step, or changes its review details (reviewers, commit, the owner's
    /// words, reports such as ["N-26"]). What you leave out stays as it was, so
    /// {"reports": ["N-26"]} only sets the reports.
    pub review: Option<ReviewInfo>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConcludeStep {
    /// Only for the consultant: which project.
    pub project: Option<String>,
    /// The step, e.g. "P9.3d", or "M4/K2" if the name repeats.
    pub step: String,
    /// A few lines in your words: what shipped, what turned up, the biggest miss against the
    /// estimates and why. AgentFlow adds the numbers, PRs and decisions itself.
    pub summary: String,
    /// Move every task still open (todo, doing, blocked) to this step. Without it, each open task
    /// has to be moved, parked or dropped first.
    pub move_open_to: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ArchiveMilestone {
    /// Only for the consultant: which project.
    pub project: Option<String>,
    /// The step ("P9.3b") or milestone ("M7").
    pub milestone: String,
    /// For a step: move its tasks (and its notes, PRs and timings) to this step instead of
    /// archiving them with it, e.g. when folding small steps into one.
    pub move_to: Option<String>,
    /// Why, in a line; kept in its history and the event log.
    pub reason: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetMilestone {
    /// A project's slug; your own if left out.
    pub project: Option<String>,
    /// The milestone or step, e.g. "P9.3" or "M9" ("M4/K2" if a step name repeats).
    pub milestone: String,
    /// Find an archived one too.
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RestoreMilestone {
    /// Only for the consultant: which project.
    pub project: Option<String>,
    /// The archived step or milestone, e.g. "P9.3b".
    pub milestone: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EditProjectDoc {
    /// Only for the consultant: which project.
    pub project: Option<String>,
    /// The `##` section to change, e.g. "Roadmap": replaced, added at the end if there's none, or
    /// removed when text is empty. Leave it out to replace the whole document.
    pub section: Option<String>,
    /// Markdown: the section's text (without its heading), or the whole document.
    pub text: String,
}

fn notes_back(c: &rusqlite::Connection, project_id: i64) -> anyhow::Result<Value> {
    let now = now_ms();
    let notes = records::query(c, &Query { project_id: Some(project_id), kinds: vec![Kind::Note], states: vec!["current".into()], limit: 0, ..Default::default() })?;
    Ok(Value::Array(
        notes
            .iter()
            .filter(|n| !n.str("resurface").is_empty())
            .filter(|n| n.data.get("resurface_at").and_then(Value::as_i64).is_none_or(|at| at <= now))
            .map(|n| json!({"id": n.short_id(), "name": n.title, "category": n.str("category"), "when": n.str("resurface")}))
            .collect(),
    ))
}

fn doc_json(d: &crate::projectdoc::Doc) -> Value {
    json!({"text": d.text, "rev": d.rev, "edited": if d.rev > 0 { format!("{} by {} ({})", crate::util::fmt_utc(d.updated_at), d.author, d.what) } else { String::new() }})
}

fn project_json(p: &projects::Project, now: i64) -> Value {
    json!({
        "project": p.slug,
        "name": p.name,
        "description": p.description,
        "status": p.status,
        "repo": p.repo,
        "checkout": p.checkout,
        "machine": p.machine,
        "model": p.model,
        "effort": p.effort,
        "ports": p.ports,
        "build_product": p.build_product,
        "agent": {
            "session": p.agent.session,
            "machine": p.agent.machine,
            "online": p.agent.online(now),
            "working_on": p.agent.note,
        },
    })
}

fn milestones_json(ms: &[projects::Milestone], counts: &[(i64, String, i64)]) -> Value {
    let count_of = |id: i64| -> Value {
        let mut o = serde_json::Map::new();
        for (m, state, n) in counts {
            if *m == id {
                o.insert(state.clone(), (*n).into());
            }
        }
        Value::Object(o)
    };
    let top: Vec<Value> = ms
        .iter()
        .filter(|m| m.parent_id.is_none())
        .map(|m| {
            let steps: Vec<Value> = ms
                .iter()
                .filter(|s| s.parent_id == Some(m.id))
                .map(|s| json!({"name": s.name, "title": s.title, "state": s.state, "tasks": count_of(s.id)}))
                .collect();
            let mut v = json!({"name": m.name, "title": m.title, "state": m.state, "tasks": count_of(m.id)});
            if !m.tag.is_empty() {
                v["tag"] = m.tag.clone().into();
            }
            if !m.description.is_empty() {
                v["description"] = m.description.clone().into();
            }
            if !steps.is_empty() {
                v["steps"] = steps.into();
            }
            v
        })
        .collect();
    top.into()
}

fn task_counts(c: &rusqlite::Connection, project_id: i64) -> anyhow::Result<Vec<(i64, String, i64)>> {
    let mut stmt = c.prepare(
        "SELECT milestone_id, state, count(*) FROM records WHERE project_id = ?1 AND kind = 'task' AND milestone_id IS NOT NULL
         AND state != 'archived' GROUP BY milestone_id, state",
    )?;
    let rows = stmt.query_map([project_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// What hello returns (the tool, and `flow hello` for a SessionStart hook): records the
/// session, then the project, its document, milestones, where it is, questions, notes and news.
pub(crate) fn hello_reply(
    c: &rusqlite::Connection,
    project: &projects::Project,
    p: &Hello,
    token_dir: &std::path::Path,
) -> anyhow::Result<Value> {
    let prev_hello = projects::hello(c, project.id, &p.session, &p.machine, &p.model, &p.effort)?;
    // Project-wide decisions since your last session: standing rules to fold into the
    // project document (Imaginarium's agent, 2026-11-12).
    let new_project_decisions: Vec<Value> = match prev_hello {
        None => Vec::new(),
        Some(since) => records::query(
            c,
            &Query { project_id: Some(project.id), kinds: vec![Kind::Decision], states: vec!["current".into()], limit: 0, ..Default::default() },
        )?
        .into_iter()
        .filter(|d| d.milestone_id.is_none() && d.created_at >= since)
        .map(|d| json!({"id": d.ref_id, "title": d.title, "owner_words": d.data.get("owner_words")}))
        .collect(),
    };
    let ms = projects::milestones(c, project.id)?;
    let counts = task_counts(c, project.id)?;
    let open = records::query(
        c,
        &Query { project_id: Some(project.id), kinds: vec![Kind::Question], states: vec!["open".into()], ..Default::default() },
    )?;
    let answered = records::query(
        c,
        &Query {
            project_id: Some(project.id),
            kinds: vec![Kind::Question],
            states: vec!["answered".into()],
            since: Some(now_ms() - 7 * 86_400_000),
            limit: 20,
            ..Default::default()
        },
    )?;
    let recent = records::recent_events(c, Some(project.id), 15)?;
    let unread = crate::waiting::unread_count(c, project.id, &project.slug)?;
    let reply = json!({
        "project": project_json(&projects::get_by_id(c, project.id)?, now_ms()),
        // Your project document: rules for working on it, stack, roadmap, pointers.
        // Keep it current with edit_project_doc; the owner may edit it too.
        "doc": doc_json(&crate::projectdoc::get(c, project.id)?),
        "milestones": milestones_json(&ms, &counts),
        // Where you are: each step under way, its tasks in doing, the next one, and
        // what's blocked by what.
        "where": crate::steps::where_json(c, project.id)?,
        // Notes that come back today (a date that's here) or until resolved (a condition).
        "notes_back": notes_back(c, project.id)?,
        "open_questions": open.iter().map(|q| json!({"id": q.ref_id, "title": q.title})).collect::<Vec<_>>(),
        "answered_this_week": answered.iter().map(|q| json!({"id": q.ref_id, "title": q.title, "answer": q.data.get("answer")})).collect::<Vec<_>>(),
        "new_project_decisions": new_project_decisions,
        "unread_events": unread,
        "recent_events": recent.iter().map(|e| json!({"at": crate::util::fmt_utc(e.at), "what": e.summary, "by": e.actor})).collect::<Vec<_>>(),
        // The CLI (publish, screenshot, contribute, remote, wait) needs the same token.
        "cli": {
            "token_file": crate::tokens::token_path(token_dir, Some(&project.slug), &project.name),
            "how": "Run flow commands with AGENTFLOW_TOKEN_FILE set to token_file (a project folder set up by AgentFlow sets it, with AGENTFLOW_URL), or add --token-file token_file."
        },
    });
    let mut reply = reply;
    if p.brief
        && let Some(o) = reply.as_object_mut()
    {
        for k in ["doc", "milestones", "cli", "project"] {
            o.remove(k);
        }
    }
    Ok(reply)
}

#[tool_router(router = project_tools, vis = "pub(super)")]
impl FlowMcp {
    #[tool(description = "Start every session with this: says which session, machine, model and effort work on your \
        project, and returns your project, its document, milestones, where you are (steps under way), open questions, \
        notes coming back and what changed since your last session. Call it again with brief: true after your context \
        was reset or summarized, to get where you are, news and notes without the document.")]
    async fn hello(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<Hello>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let token_dir = self.state.config.token_dir();
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let project = target_project(c, &a, None, true)?;
                let reply = hello_reply(c, &project, &p, &token_dir)?;
                let what = if p.brief { "hello (brief)" } else { "hello" };
                Ok(Outcome::new(reply).summary(format!("{what} from {} on {}", p.session, p.machine)).project(project.slug))
            })
            .await;
        self.finish(&actor, "hello", Action::Read, outcome).await
    }

    #[tool(description = "What happened in your project since you last checked that you didn't do yourself: \
        the owner's answers (and the decisions they made), button presses, messages, feedback. Marks them read; the \
        replies of your other write calls carry the same news as it comes. again: \"1h\" shows what came in the last \
        hour once more, read or not. Also see `flow wait` to block until something happens.")]
    async fn since_last_check(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<SinceLastCheck>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let project = target_project(c, &a, None, true)?;
                let events = match p.again.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    Some(w) => {
                        let min = crate::steps::parse_minutes(w)
                            .ok_or_else(|| anyhow::anyhow!("again is a duration, e.g. \"1h\" or \"30 min\""))?;
                        crate::waiting::news_again(c, project.id, &project.slug, min * 60_000)?
                    }
                    None => crate::waiting::read_news(c, project.id, &project.slug)?,
                };
                let more = events.len() as i64 == crate::waiting::UNREAD_PAGE;
                let list: Vec<Value> = events
                    .iter()
                    .map(|e| json!({"at": crate::util::fmt_utc(e.at), "record": e.record, "what": e.summary, "by": e.actor}))
                    .collect();
                Ok(Outcome::new(json!({"events": list, "more": more})).summary(format!("since_last_check: {} events", events.len())).project(project.slug))
            })
            .await;
        self.finish(&actor, "since_last_check", Action::Read, outcome).await
    }

    #[tool(
        description = "All projects: name, what it is, status, whether its agent is online. Every agent can read this; project_info has a project's document."
    )]
    async fn list_projects(&self, Extension(parts): Extension<Parts>) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let outcome = self
            .state
            .db
            .call(move |c| {
                let now = now_ms();
                let list: Vec<Value> = projects::list(c)?
                    .iter()
                    .map(|p| {
                        json!({"project": p.slug, "name": p.name, "description": p.description,
                               "status": p.status, "machine": p.machine, "agent_online": p.agent.online(now)})
                    })
                    .collect();
                Ok(Outcome::new(json!({ "projects": list })).summary("list_projects"))
            })
            .await;
        self.finish(&actor, "list_projects", Action::Read, outcome).await
    }

    #[tool(
        description = "A project's details, its document and milestones with task counts. Your own project if you leave `project` out."
    )]
    async fn project_info(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ProjectArg>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let project = target_project(c, &a, p.project.as_deref(), false)?;
                let ms = projects::milestones(c, project.id)?;
                let counts = task_counts(c, project.id)?;
                let reply = json!({"project": project_json(&project, now_ms()), "milestones": milestones_json(&ms, &counts),
                    "doc": doc_json(&crate::projectdoc::get(c, project.id)?)});
                Ok(Outcome::new(reply).summary(format!("project_info {}", project.slug)).project(project.slug))
            })
            .await;
        self.finish(&actor, "project_info", Action::Read, outcome).await
    }

    #[tool(description = "Change your project's details: description, repo, checkout, machine, default model and \
        effort, ports, build product. Everything else about the project (stack, platform, git and PM rules, roadmap) \
        goes in its document: edit_project_doc.")]
    async fn update_project(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<UpdateProject>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let project = target_project(&tx, &a, p.project.as_deref(), true)?;
                let changed = projects::update(&tx, project.id, &p.patch, &a.name())?;
                tx.commit()?;
                Ok(Outcome::new(json!({"changed": changed}))
                    .summary(format!(
                        "update_project: {}",
                        if changed.is_empty() { "nothing".into() } else { changed.join(", ") }
                    ))
                    .project(project.slug))
            })
            .await;
        self.finish(&actor, "update_project", Action::Write, outcome).await
    }

    #[tool(description = "Only when it says more than your tasks in doing (the page shows those by itself): one line on \
        what you're doing right now, for the project page. Cleared at your next hello.")]
    async fn working_on(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<WorkingOn>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let project = target_project(c, &a, None, true)?;
                projects::set_agent_note(c, project.id, &p.note)?;
                Ok(Outcome::new(json!({"ok": true}))
                    .summary(format!("working on: {}", p.note.trim()))
                    .project(project.slug))
            })
            .await;
        self.finish(&actor, "working_on", Action::Write, outcome).await
    }

    #[tool(description = "Add a milestone (M, A or P) or a step inside one. Every milestone ends with a tag; \
        milestones span the whole project and are set at the start, so add them in order.")]
    async fn add_milestone(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<AddMilestone>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let project = target_project(&tx, &a, p.project.as_deref(), true)?;
                let parent = match &p.step_of {
                    Some(name) => Some(projects::find_milestone(&tx, project.id, name)?.id),
                    None => None,
                };
                let before = match &p.before {
                    Some(name) => Some(projects::find_milestone(&tx, project.id, name)?.id),
                    None => None,
                };
                let m = projects::add_milestone(
                    &tx,
                    project.id,
                    &NewMilestone { name: &p.name, title: &p.title, description: &p.description, parent, before },
                    &a.name(),
                )?;
                let m = if p.state.is_some() || p.started_at.is_some() || p.closed_at.is_some() || p.summary.is_some() {
                    let set = MilestonePatch {
                        state: p.state.clone(),
                        started_at: p.started_at.clone(),
                        closed_at: p.closed_at.clone(),
                        summary: p.summary.clone(),
                        ..Default::default()
                    };
                    projects::update_milestone(&tx, &m, &set, &a.name())?
                } else {
                    m
                };
                if let Some(r) = &p.review {
                    if parent.is_none() {
                        anyhow::bail!("a review is a step: give step_of, the milestone it reviews");
                    }
                    if r.reviewers.is_empty() || r.commit.trim().is_empty() {
                        anyhow::bail!("a review step names its reviewers and the commit they review");
                    }
                    projects::set_review(
                        &tx,
                        &m,
                        &json!({"reviewers": r.reviewers, "commit": r.commit.trim(), "owner_words": r.owner_words.as_deref().unwrap_or("").trim(), "reports": r.reports.clone().unwrap_or_default()}),
                    )?;
                }
                tx.commit()?;
                Ok(Outcome::new(json!({"added": m.name, "title": m.title, "state": m.state}))
                    .summary(format!("milestone {} added", m.name))
                    .project(project.slug))
            })
            .await;
        self.finish(&actor, "add_milestone", Action::Write, outcome).await
    }

    #[tool(
        description = "Change a milestone or step: rename, retitle, describe, move it (move_before, or step_of to \
        another milestone), set its state (planned, active, closed), dates, summary and the tag that closed it. \
        Milestones and steps are yours to edit freely, closed ones too (the owner, 2026-11-12); archive_milestone \
        puts one away."
    )]
    async fn update_milestone(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<UpdateMilestone>,
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
                let m = projects::update_milestone(&tx, &m, &p.patch, &a.name())?;
                if let Some(r) = &p.review {
                    if m.parent_id.is_none() {
                        anyhow::bail!("a review is a step; {} is a milestone", m.name);
                    }
                    let review = r.over(&m.review);
                    if review["reviewers"].as_array().is_none_or(|a| a.is_empty()) || review["commit"].as_str().is_none_or(|c| c.is_empty()) {
                        anyhow::bail!("a review step names its reviewers and the commit they review");
                    }
                    projects::set_review(&tx, &m, &review)?;
                }
                tx.commit()?;
                Ok(Outcome::new(json!({"milestone": m.name, "title": m.title, "state": m.state, "tag": m.tag, "summary": m.summary,
                    "started_at": m.started_at.map(crate::util::fmt_utc), "closed_at": m.closed_at.map(crate::util::fmt_utc)}))
                    .summary(format!("milestone {} updated", m.name))
                    .project(project.slug))
            })
            .await;
        self.finish(&actor, "update_milestone", Action::Write, outcome).await
    }

    #[tool(description = "Conclude a step once its tasks are done (open PRs and untested builds don't hold it: \
        stack them and move on). Give a few lines on what it brought; AgentFlow adds the numbers, PRs and the owner's \
        decisions. Tasks still open must be moved, parked or dropped first, or all moved with move_open_to. Steps start \
        by themselves when their first task goes to doing.")]
    async fn conclude_step(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ConcludeStep>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let project = target_project(&tx, &a, p.project.as_deref(), true)?;
                let step = projects::find_milestone(&tx, project.id, &p.step)?;
                let to = match p.move_open_to.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    Some(n) => Some(projects::find_milestone(&tx, project.id, n)?),
                    None => None,
                };
                let done = crate::steps::conclude(&tx, &step, &p.summary, to.as_ref(), &a.name())?;
                tx.commit()?;
                Ok(Outcome::new(json!({"step": done.step.name, "state": "concluded", "moved": done.moved}))
                    .summary(format!("step {} concluded", done.step.name))
                    .project(project.slug))
            })
            .await;
        self.finish(&actor, "conclude_step", Action::Write, outcome).await
    }

    #[tool(description = "Archive a step, or a milestone with its steps (the owner, 2026-11-12: agents add, edit \
        and archive milestones, steps, tasks, decisions and questions freely; nothing is deleted). Archived ones are \
        hidden from the lists and counts, reachable with include_archived, and restore_milestone brings them back. A \
        step's tasks go with it, or to move_to (with its notes, PRs and timings), e.g. to fold small steps into one: \
        add a done task per old step to the target first (add_tasks with started_at and done_at), then archive each \
        with move_to. A milestone takes its steps, tasks and the decisions and questions scoped to it along; \
        project-wide decisions stay. Archived names stay taken.")]
    async fn archive_milestone(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ArchiveMilestone>,
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
                let to = match p.move_to.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
                    Some(name) => Some(projects::find_milestone(&tx, project.id, name)?),
                    None => None,
                };
                let done = crate::steps::archive_milestone(&tx, &m, to.as_ref(), &p.reason, &a.name())?;
                tx.commit()?;
                Ok(Outcome::new(json!({"archived": m.name, "steps": done.steps, "archived_with_it": done.archived,
                    "moved": done.moved, "moved_to": to.map(|t| t.name)}))
                    .summary(format!("{} archived", m.name))
                    .project(project.slug))
            })
            .await;
        self.finish(&actor, "archive_milestone", Action::Write, outcome).await
    }

    #[tool(description = "A milestone or step in full (Imaginarium's agent, 2026-11-12: steps couldn't be read \
        back): its title, description, state, dates, summary, tag, review details, steps, its tasks in order, and the \
        notes and decisions that belong to it.")]
    async fn get_milestone(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<GetMilestone>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let project = target_project(c, &a, p.project.as_deref(), false)?;
                let m = projects::find_milestone_any(c, project.id, &p.milestone, p.include_archived)?;
                let mut all = projects::milestones(c, project.id)?;
                if p.include_archived {
                    all.extend(projects::archived_milestones(c, project.id)?);
                }
                let parent = m.parent_id.and_then(|id| all.iter().find(|x| x.id == id).map(|x| x.name.clone()));
                let steps: Vec<Value> = all
                    .iter()
                    .filter(|s| s.parent_id == Some(m.id))
                    .map(|s| json!({"name": s.name, "title": s.title, "state": s.state, "archived": s.archived_at.is_some()}))
                    .collect();
                let mut tasks = records::query(
                    c,
                    &Query { project_id: Some(project.id), kinds: vec![Kind::Task], include_archived: p.include_archived, limit: 0, ..Default::default() },
                )?;
                tasks.retain(|t| t.milestone_id == Some(m.id));
                crate::steps::sort_by_position(&mut tasks);
                let belongs = records::query(
                    c,
                    &Query { project_id: Some(project.id), kinds: vec![Kind::Note, Kind::Decision, Kind::Question], include_archived: p.include_archived, limit: 0, ..Default::default() },
                )?;
                let fmt = |t: Option<i64>| t.map(crate::util::fmt_utc);
                let reply = json!({
                    "name": m.name, "title": m.title, "description": m.description, "state": m.state,
                    "step_of": parent, "tag": m.tag, "started_at": fmt(m.started_at), "closed_at": fmt(m.closed_at),
                    "summary": m.summary,
                    "review": serde_json::from_str::<Value>(&m.review).unwrap_or(Value::Null),
                    "archived": m.archived_at.map(|at| json!({"at": crate::util::fmt_utc(at), "reason": m.archived_reason})),
                    "steps": steps,
                    "tasks": tasks.iter().map(|t| json!({"id": t.short_id(), "title": t.title, "state": t.state,
                        "estimate": t.data.get("estimate"), "started_at": t.data.get("started_at").and_then(Value::as_i64).map(crate::util::fmt_utc),
                        "done_at": t.data.get("done_at").and_then(Value::as_i64).map(crate::util::fmt_utc)})).collect::<Vec<_>>(),
                    "notes": belongs.iter().filter(|r| r.kind == Kind::Note && r.milestone_id == Some(m.id)).map(|r| json!({"id": r.short_id(), "title": r.title})).collect::<Vec<_>>(),
                    "scoped_here": belongs.iter().filter(|r| r.kind != Kind::Note && r.milestone_id == Some(m.id)).map(|r| json!({"id": r.short_id(), "title": r.title, "state": r.state})).collect::<Vec<_>>(),
                });
                Ok(Outcome::new(reply).summary(format!("get_milestone {}", m.name)).project(project.slug))
            })
            .await;
        self.finish(&actor, "get_milestone", Action::Read, outcome).await
    }

    #[tool(description = "Bring back an archived step or milestone, with what was archived along with it (its \
        steps, tasks, decisions and questions). Tasks moved elsewhere when it was archived stay where they are.")]
    async fn restore_milestone(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<RestoreMilestone>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let project = target_project(&tx, &a, p.project.as_deref(), true)?;
                let m = projects::find_milestone_any(&tx, project.id, &p.milestone, true)?;
                let back = crate::steps::restore_milestone(&tx, &m, &a.name())?;
                tx.commit()?;
                Ok(Outcome::new(json!({"restored": m.name, "with": back})).summary(format!("{} restored", m.name)).project(project.slug))
            })
            .await;
        self.finish(&actor, "restore_milestone", Action::Write, outcome).await
    }

    #[tool(description = "Change your project document: one `##` section at a time (replaced, added or removed), \
        or the whole document. It holds what's specific to the project: how to work on it (commit rules, tests, \
        tools), tech stack and platform, the roadmap if it has one, pointers. hello returns it in full, so keep it \
        short (16 KB at most) and current; long material goes into notes. The owner reads and may edit it.")]
    async fn edit_project_doc(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<EditProjectDoc>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let project = target_project(&tx, &a, p.project.as_deref(), true)?;
                let old = crate::projectdoc::get(&tx, project.id)?;
                let (text, what) = match p.section.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    Some(sec) => {
                        let what = if p.text.trim().is_empty() { format!("removed section {sec}") } else { format!("section {sec}") };
                        (crate::projectdoc::edit_section(&old.text, sec, &p.text)?, what)
                    }
                    None => (p.text.clone(), "the whole document".to_owned()),
                };
                let (d, changed) = crate::projectdoc::set(&tx, project.id, &text, &a.name(), &what)?;
                tx.commit()?;
                Ok(Outcome::new(json!({"rev": d.rev, "changed": changed, "bytes": d.text.len()}))
                    .summary(format!("project document: {what}"))
                    .project(project.slug))
            })
            .await;
        self.finish(&actor, "edit_project_doc", Action::Write, outcome).await
    }
}
