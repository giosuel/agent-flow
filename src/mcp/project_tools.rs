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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
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
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReviewInfo {
    /// Who reviews, e.g. ["correctness", "UX", "performance"] (fresh agents, no context of the work).
    pub reviewers: Vec<String>,
    /// The commit reviewed, e.g. "9f3c2e1" (on main, or say the branch).
    pub commit: String,
    /// The owner's words for the review, verbatim, if they gave any.
    #[serde(default)]
    pub owner_words: String,
    /// The reports, as notes or links, e.g. ["N-6"].
    #[serde(default)]
    pub reports: Vec<String>,
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
    /// words, reports such as ["N-26"]).
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
         GROUP BY milestone_id, state",
    )?;
    let rows = stmt.query_map([project_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
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
                projects::hello(c, project.id, &p.session, &p.machine, &p.model, &p.effort)?;
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
                    "unread_events": unread,
                    "recent_events": recent.iter().map(|e| json!({"at": crate::util::fmt_utc(e.at), "what": e.summary, "by": e.actor})).collect::<Vec<_>>(),
                    // The CLI (publish, screenshot, contribute, remote, wait) needs the same token.
                    "cli": {
                        "token_file": crate::tokens::token_path(&token_dir, Some(&project.slug), &project.name),
                        "how": "Run agentflow commands with AGENTFLOW_TOKEN_FILE set to token_file (sessions started with AgentFlow's start command have it), or add --token-file token_file."
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
                let what = if p.brief { "hello (brief)" } else { "hello" };
                Ok(Outcome::new(reply).summary(format!("{what} from {} on {}", p.session, p.machine)).project(project.slug))
            })
            .await;
        self.finish(&actor, "hello", Action::Read, outcome).await
    }

    #[tool(description = "What happened in your project since you last checked that you didn't do yourself: \
        the owner's answers, button presses, messages, feedback. Marks them read. Also see `agentflow wait` to block until \
        something happens.")]
    async fn since_last_check(&self, Extension(parts): Extension<Parts>) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let project = target_project(c, &a, None, true)?;
                let events = crate::waiting::unread(c, project.id, &project.slug)?;
                if let Some(last) = events.last() {
                    crate::waiting::set_cursor(c, project.id, last.seq)?;
                }
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
                        &json!({"reviewers": r.reviewers, "commit": r.commit.trim(), "owner_words": r.owner_words.trim(), "reports": r.reports}),
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
        description = "Change a milestone or step: rename, retitle, describe, move it (move_before), set its state \
        (planned, active, closed) and the tag that closed it. A closed milestone should stay closed; reopening needs a reason."
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
                    if r.reviewers.is_empty() || r.commit.trim().is_empty() {
                        anyhow::bail!("a review step names its reviewers and the commit they review");
                    }
                    projects::set_review(
                        &tx,
                        &m,
                        &json!({"reviewers": r.reviewers, "commit": r.commit.trim(), "owner_words": r.owner_words.trim(), "reports": r.reports}),
                    )?;
                }
                tx.commit()?;
                Ok(Outcome::new(json!({"milestone": m.name, "title": m.title, "state": m.state, "tag": m.tag}))
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
