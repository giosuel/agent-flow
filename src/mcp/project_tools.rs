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
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateMilestone {
    /// Only for the consultant: which project.
    pub project: Option<String>,
    /// The milestone or step, e.g. "M4", or "M4/K2" if the step name repeats.
    pub milestone: String,
    #[serde(flatten)]
    pub patch: MilestonePatch,
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
        "stack": p.stack,
        "platform": p.platform,
        "model": p.model,
        "effort": p.effort,
        "pm_rules": p.pm_rules,
        "git_rules": p.git_rules,
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
        project, and returns your project, its milestones, open questions and what changed since your last session.")]
    async fn hello(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<Hello>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
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
                let unread = crate::waiting::unread(c, project.id, &project.slug)?.len();
                let reply = json!({
                    "project": project_json(&projects::get_by_id(c, project.id)?, now_ms()),
                    "milestones": milestones_json(&ms, &counts),
                    "open_questions": open.iter().map(|q| json!({"id": q.ref_id, "title": q.title})).collect::<Vec<_>>(),
                    "answered_this_week": answered.iter().map(|q| json!({"id": q.ref_id, "title": q.title, "answer": q.data.get("answer")})).collect::<Vec<_>>(),
                    "unread_events": unread,
                    "recent_events": recent.iter().map(|e| json!({"at": crate::util::fmt_utc(e.at), "what": e.summary, "by": e.actor})).collect::<Vec<_>>(),
                });
                Ok(Outcome::new(reply).summary(format!("hello from {} on {}", p.session, p.machine)).project(project.slug))
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
                crate::waiting::set_cursor(c, project.id, crate::waiting::latest_seq(c)?)?;
                let list: Vec<Value> = events
                    .iter()
                    .map(|e| json!({"at": crate::util::fmt_utc(e.at), "record": e.record, "what": e.summary, "by": e.actor}))
                    .collect();
                Ok(Outcome::new(json!({"events": list})).summary(format!("since_last_check: {} events", events.len())).project(project.slug))
            })
            .await;
        self.finish(&actor, "since_last_check", Action::Read, outcome).await
    }

    #[tool(
        description = "All projects: name, what it is, stack, status, whether its agent is online. Every agent can read this."
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
                        json!({"project": p.slug, "name": p.name, "description": p.description, "stack": p.stack,
                               "status": p.status, "machine": p.machine, "agent_online": p.agent.online(now)})
                    })
                    .collect();
                Ok(Outcome::new(json!({ "projects": list })).summary("list_projects"))
            })
            .await;
        self.finish(&actor, "list_projects", Action::Read, outcome).await
    }

    #[tool(
        description = "A project's details and milestones with task counts. Your own project if you leave `project` out."
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
                let reply =
                    json!({"project": project_json(&project, now_ms()), "milestones": milestones_json(&ms, &counts)});
                Ok(Outcome::new(reply).summary(format!("project_info {}", project.slug)).project(project.slug))
            })
            .await;
        self.finish(&actor, "project_info", Action::Read, outcome).await
    }

    #[tool(description = "Change your project's details: description, repo, checkout, machine, stack, platform, \
        default model and effort, PM rules that override the defaults, git rules, ports, viber-run product.")]
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

    #[tool(description = "Say what you're working on right now, in one line, for the project page.")]
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
                tx.commit()?;
                Ok(Outcome::new(json!({"milestone": m.name, "title": m.title, "state": m.state, "tag": m.tag}))
                    .summary(format!("milestone {} updated", m.name))
                    .project(project.slug))
            })
            .await;
        self.finish(&actor, "update_milestone", Action::Write, outcome).await
    }
}
