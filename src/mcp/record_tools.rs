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
    pub title: String,
    /// Details, acceptance criteria, paths.
    #[serde(default)]
    pub body: String,
    /// The milestone or step it belongs to, e.g. "M4" or "K2". Every task belongs to one.
    pub milestone: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Starting state: todo (default), doing, blocked or parked.
    pub state: Option<String>,
    /// Your own estimate, e.g. "2 h" or "40 min"; shown with the task.
    pub estimate: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateTask {
    /// The task, e.g. "T-14" or "vinculum/T-14".
    pub id: String,
    /// todo, doing, review, done, blocked or parked. Set doing when you start it.
    pub state: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    /// Move it to another milestone or step.
    pub milestone: Option<String>,
    pub tags: Option<Vec<String>>,
    /// For parked tasks: when it should come back, e.g. "after M5".
    pub resurface_when: Option<String>,
    pub estimate: Option<String>,
    /// A line for the task's history, e.g. why it's blocked.
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
    /// Records this question blocks, e.g. ["T-18", "PR-56"].
    #[serde(default)]
    pub blocks: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Send it as an urgent notification. Only when waiting would really cost something.
    #[serde(default)]
    pub urgent: bool,
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

#[tool_router(router = record_tools, vis = "pub(super)")]
impl FlowMcp {
    #[tool(description = "Add a task to a milestone or step of your project.")]
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
                let mut data = Map::new();
                if let Some(e) = p.estimate.filter(|e| !e.trim().is_empty()) {
                    data.insert("estimate".into(), e.trim().into());
                }
                let r = records::create(
                    &tx,
                    project.id,
                    &a.name(),
                    NewRecord {
                        kind: Some(Kind::Task),
                        title: p.title,
                        body: p.body,
                        state: p.state,
                        milestone_id: Some(m.id),
                        tags: p.tags,
                        data,
                        ..Default::default()
                    },
                )?;
                tx.commit()?;
                Ok(Outcome::new(brief(&r))
                    .summary(format!("task {} added", r.short_id()))
                    .project(project.slug)
                    .target(r.ref_id))
            })
            .await;
        self.finish(&actor, "add_task", Action::Write, outcome).await
    }

    #[tool(description = "Change a task: its state (todo, doing, review, done, blocked, parked), title, details, \
        milestone, tags, estimate, or when a parked task comes back. Add a note to explain.")]
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
                let tx = c.transaction()?;
                let r = writable(&tx, &a, &p.id)?;
                if r.kind != Kind::Task {
                    anyhow::bail!("{} isn't a task", r.ref_id);
                }
                let milestone_id = match &p.milestone {
                    Some(name) => Some(Some(projects::find_milestone(&tx, r.project_id, name)?.id)),
                    None => None,
                };
                let mut data = Map::new();
                if let Some(w) = p.resurface_when {
                    data.insert(
                        "resurface_when".into(),
                        if w.trim().is_empty() { Value::Null } else { w.trim().into() },
                    );
                }
                if let Some(e) = p.estimate {
                    data.insert("estimate".into(), if e.trim().is_empty() { Value::Null } else { e.trim().into() });
                }
                // Started now only if it wasn't already in progress.
                if p.state.as_deref() == Some("doing") && r.state != "doing" {
                    data.insert("started_at".into(), now_ms().into());
                }
                let (r, changed) = records::update(
                    &tx,
                    &r,
                    &a.name(),
                    Patch {
                        title: p.title,
                        body: p.body,
                        state: p.state,
                        milestone_id,
                        tags: p.tags,
                        data,
                        note: p.note,
                        ..Default::default()
                    },
                )?;
                tx.commit()?;
                let summary = format!("task {} {}", r.short_id(), if changed { "updated" } else { "unchanged" });
                Ok(Outcome::new(brief(&r)).summary(summary).project(r.project.clone()).target(r.ref_id))
            })
            .await;
        self.finish(&actor, "update_task", Action::Write, outcome).await
    }

    #[tool(description = "Ask the owner a question. One question per call, with the context they need, numbered \
        options and your recommendation when it's a choice. They answer on their phone or the dashboard; the answer is \
        stored verbatim and shows up in hello and get_record. Don't ask in chat as well.")]
    async fn ask_question(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<AskQuestion>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let urgent = p.urgent;
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
                    records::link(&tx, &q, &target, "blocks", &a.name())?;
                }
                tx.commit()?;
                Ok((
                    Outcome::new(json!({"id": q.ref_id, "state": "open"}))
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
                self.state.push.spawn(
                    kind,
                    Notification {
                        title: format!("{name} asks"),
                        body: q.title.clone(),
                        url: q.url(),
                        tag: Some(q.ref_id.clone()),
                    },
                );
                Ok(o)
            }
            Err(e) => Err(e),
        };
        self.finish(&actor, "ask_question", Action::Write, outcome).await
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
