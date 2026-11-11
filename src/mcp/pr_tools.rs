//! Tools for pull requests, releases and the owner's buttons.

use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{FlowMcp, Outcome, actor_of, target_project, writable};
use crate::activity::Action;
use crate::records::{self, Kind, NewRecord};
use crate::{actions, prs};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdatePr {
    /// The PR's number on GitHub, in your project's repository.
    pub number: i64,
    /// True when it's ready for the owner to merge; false takes that back.
    pub ready: Option<bool>,
    /// Tasks this PR carries out, e.g. ["T-14"].
    #[serde(default)]
    pub implements: Vec<String>,
    /// A line for the PR's history.
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ProjectArg {
    /// A project's slug; your own if left out.
    pub project: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddRelease {
    /// The tag, e.g. "v0.0.5-m4" (milestones) or "v1.3.0".
    pub tag: String,
    /// e.g. "M4: Keybinds"
    pub title: String,
    /// PRs that must be merged before tagging, e.g. ["PR-48", "PR-57"].
    #[serde(default)]
    pub waits_for: Vec<String>,
    /// Release notes or what the tag closes, in Markdown.
    #[serde(default)]
    pub body: String,
    /// The milestone this tag closes, e.g. "M4".
    pub milestone: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AddButton {
    /// The PR or release, e.g. "PR-58" or "R-3".
    pub record: String,
    /// What the owner sees on the button: "Merge", "Push tag v0.0.5-m4".
    pub label: String,
    /// "merge" for a PR's merge (unlocks in merge order, and "Merge #a–#b" uses it), "tag" for a
    /// release's tag (marks the release tagged once it ran), "other" for anything else.
    pub kind: String,
    /// The exact commands, run with bash when the owner presses it, e.g.
    /// "gh pr merge 58 -R giosuel/vinculum --merge --delete-branch". In a stack, delete the
    /// branch on merge so GitHub moves the next PR onto main. Never run them yourself.
    pub command: String,
    /// The folder to run in, e.g. "~/vinculum". Your home if left out.
    #[serde(default)]
    pub workdir: String,
    /// Records that must be merged (or done) before the button unlocks, e.g. ["PR-57"].
    #[serde(default)]
    pub requires: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UpdateBuild {
    /// The build, e.g. "B-27".
    pub id: String,
    /// What the owner should try, each "Title: what to do". Replaces the list; with at least one,
    /// the build waits for the owner's feedback (and they're notified).
    pub features: Option<Vec<String>>,
    /// Set or clear (empty) the tag this build is, e.g. "v0.0.5-m4".
    pub tag: Option<String>,
    /// Take it back: the owner shouldn't try this one.
    #[serde(default)]
    pub withdraw: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ButtonId {
    /// The button's id, from pull_requests or get_record.
    pub id: i64,
}

#[tool_router(router = pr_tools, vis = "pub(super)")]
impl FlowMcp {
    #[tool(
        description = "Mark one of your PRs ready for the owner to merge (or not), and say which tasks it carries out. \
        AgentFlow picks up PRs from your repository by itself; this also adds one it hasn't seen yet."
    )]
    async fn update_pr(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<UpdatePr>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let project = {
            let a = a.clone();
            self.state.db.call(move |c| target_project(c, &a, None, true)).await
        };
        let outcome = async {
            let project = project?;
            if project.repo.is_empty() {
                anyhow::bail!("your project has no repository yet; set it with update_project");
            }
            // Fetch it now, so a brand-new PR is there without waiting for the poller.
            let info = self.state.github.pr(&project.repo, p.number).await?;
            crate::poller::apply(&self.state, &project, &project.repo, info).await?;
            let (proj, name) = (project.clone(), project.name.clone());
            let (r, newly_ready) = self
                .state
                .db
                .call(move |c| {
                    let tx = c.transaction()?;
                    let mut r = records::find(
                        &tx,
                        &records::RecordRef { project: Some(proj.slug.clone()), kind: Kind::Pr, number: p.number },
                        None,
                    )?;
                    let was = prs::merge_ready(&r).is_ok();
                    if let Some(ready) = p.ready {
                        r = prs::set_ready(&tx, &r, ready, &p.note, &a.name())?;
                    }
                    for t in &p.implements {
                        let task = records::find_str(&tx, t, Some(&proj.slug))?;
                        records::link(&tx, &r, &task, "implements", &a.name())?;
                    }
                    let r = records::get(&tx, r.id)?;
                    tx.commit()?;
                    let now = prs::merge_ready(&r).is_ok();
                    Ok((r, !was && now))
                })
                .await?;
            if newly_ready {
                crate::poller::notify_ready(&self.state, &name, &r);
            }
            let why = prs::merge_ready(&r).err();
            Ok(Outcome::new(
                json!({"id": r.ref_id, "state": r.state, "ready": r.data.get("ready"), "not_mergeable_because": why}),
            )
            .summary(format!("PR {} updated", r.short_id()))
            .project(r.project.clone())
            .target(r.ref_id))
        }
        .await;
        self.finish(&actor, "update_pr", Action::Write, outcome).await
    }

    #[tool(description = "A project's open PRs in merge order (from their bases), each with whether it can be merged \
        and why not, and its buttons; plus releases waiting to be tagged.")]
    async fn pull_requests(
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
                let buttons = |c: &rusqlite::Connection, id: i64| -> anyhow::Result<Vec<Value>> {
                    let mut out = Vec::new();
                    for b in actions::for_record(c, id)? {
                        let why = actions::enabled(c, &b)?.err();
                        out.push(json!({"id": b.id, "label": b.label, "state": b.state, "locked_because": why, "exit_code": b.exit_code}));
                    }
                    Ok(out)
                };
                let mut list = Vec::new();
                for o in prs::merge_order(c, project.id)? {
                    let r = &o.record;
                    list.push(json!({
                        "position": o.position, "id": r.ref_id, "title": r.title, "after": o.after.map(|n| format!("PR-{n}")),
                        "ci": r.str("ci"), "ready": r.data.get("ready"), "mergeable_now": prs::merge_ready(r).is_ok() && o.after.is_none(),
                        "not_mergeable_because": prs::merge_ready(r).err(), "buttons": buttons(c, r.id)?,
                    }));
                }
                let mut releases = Vec::new();
                for r in records::query(c, &records::Query { project_id: Some(project.id), kinds: vec![Kind::Release], states: vec!["waiting".into(), "planned".into()], ..Default::default() })? {
                    releases.push(json!({"id": r.ref_id, "tag": r.str("tag"), "title": r.title, "waits_for": r.data.get("waits_for"),
                        "all_merged": crate::prs::release_ready(c, &r)?, "buttons": buttons(c, r.id)?}));
                }
                Ok(Outcome::new(json!({"merge_order": list, "releases": releases})).summary("pull_requests").project(project.slug))
            })
            .await;
        self.finish(&actor, "pull_requests", Action::Read, outcome).await
    }

    #[tool(
        description = "Plan a tag (a milestone's closing tag or a release) that waits for PRs to be merged. Then add \
        its \"Push tag …\" button with add_button; the owner presses it once every PR is merged."
    )]
    async fn add_release(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<AddRelease>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let project = target_project(&tx, &a, None, true)?;
                let tag = p.tag.trim();
                if tag.is_empty() || tag.contains(char::is_whitespace) {
                    anyhow::bail!("tag: one word, e.g. v0.0.5-m4");
                }
                let mut waits = Vec::new();
                for w in &p.waits_for {
                    let r = records::find_str(&tx, w, Some(&project.slug))?;
                    if r.kind != Kind::Pr {
                        anyhow::bail!("{} isn't a PR", r.ref_id);
                    }
                    waits.push(Value::from(r.ref_id));
                }
                let milestone_id = match &p.milestone {
                    Some(m) => Some(crate::projects::find_milestone(&tx, project.id, m)?.id),
                    None => None,
                };
                let mut data = Map::new();
                data.insert("tag".into(), tag.into());
                data.insert("waits_for".into(), waits.into());
                let r = records::create(
                    &tx,
                    project.id,
                    &a.name(),
                    NewRecord {
                        kind: Some(Kind::Release),
                        title: p.title,
                        body: p.body,
                        state: Some("waiting".into()),
                        milestone_id,
                        data,
                        ..Default::default()
                    },
                )?;
                tx.commit()?;
                Ok(Outcome::new(json!({"id": r.ref_id, "tag": tag}))
                    .summary(format!("release {} planned: {tag}", r.short_id()))
                    .project(project.slug)
                    .target(r.ref_id))
            })
            .await;
        self.finish(&actor, "add_release", Action::Write, outcome).await
    }

    #[tool(
        description = "Put a button on one of your PRs or releases: the exact git/gh commands the owner runs by pressing it \
        (merge, push a tag). The owner sees the command first; pressing it is their approval. Never merge or tag yourself. \
        At most 40 lines and 2000 characters, since the owner reads all of it. \
        Buttons can't be edited; withdraw one and add a new one instead."
    )]
    async fn add_button(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<AddButton>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let r = writable(&tx, &a, &p.record)?;
                let kind = actions::ButtonKind::parse(&p.kind)?;
                let button = actions::NewButton {
                    label: &p.label,
                    kind,
                    command: &p.command,
                    workdir: &p.workdir,
                    requires: &p.requires,
                };
                let id = actions::add(&tx, &r, &button, &a.name())?;
                tx.commit()?;
                Ok(Outcome::new(json!({"button": id, "on": r.ref_id}))
                    .summary(format!("button \"{}\" added to {}", p.label.trim(), r.short_id()))
                    .project(r.project.clone())
                    .target(r.ref_id))
            })
            .await;
        self.finish(&actor, "add_button", Action::Write, outcome).await
    }

    #[tool(description = "Your project's test builds, newest first: number, name, what's in it, whether the owner \
        tried it, and their feedback. Publish new ones with `agentflow publish` (a command, not a tool).")]
    async fn builds(
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
                let mut list = records::query(c, &records::Query { project_id: Some(project.id), kinds: vec![Kind::Build], limit: 30, ..Default::default() })?;
                list.sort_by_key(|r| std::cmp::Reverse(r.number));
                let mut out = Vec::new();
                for r in list {
                    let feedback: Vec<Value> = records::links_of(c, &r)?
                        .into_iter()
                        .filter(|l| l.relation == "feedback_on" && !l.outgoing)
                        .map(|l| json!({"id": l.other.ref_id, "feature": l.other.title, "verdict": l.other.state, "owner_words": l.other.str("owner_words")}))
                        .collect();
                    out.push(json!({"id": r.ref_id, "name": r.str("name"), "summary": r.title, "state": r.state,
                        "features": r.data.get("features"), "tag": r.data.get("tag"), "feedback": feedback}));
                }
                Ok(Outcome::new(json!({"builds": out})).summary("builds").project(project.slug))
            })
            .await;
        self.finish(&actor, "builds", Action::Read, outcome).await
    }

    #[tool(description = "Change one of your builds: what the owner should try (features), its tag, or withdraw it.")]
    async fn update_build(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<UpdateBuild>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let r = writable(&tx, &a, &p.id)?;
                if r.kind != Kind::Build {
                    anyhow::bail!("{} isn't a build", r.ref_id);
                }
                let mut data = Map::new();
                let mut newly_ready = false;
                if let Some(fs) = &p.features {
                    let list: Vec<Value> = fs
                        .iter()
                        .filter(|f| !f.trim().is_empty())
                        .map(|f| {
                            let (t, w) = f.split_once(':').map(|(a, b)| (a.trim(), b.trim())).unwrap_or((f.trim(), ""));
                            json!({"title": t, "what_to_try": w})
                        })
                        .collect();
                    let ready = !list.is_empty() && r.state == "published";
                    newly_ready = ready && !r.data.get("ready_for_feedback").and_then(Value::as_bool).unwrap_or(false);
                    data.insert("features".into(), list.into());
                    data.insert("ready_for_feedback".into(), ready.into());
                }
                if let Some(t) = &p.tag {
                    data.insert("tag".into(), if t.trim().is_empty() { Value::Null } else { t.trim().into() });
                }
                let state = p.withdraw.then(|| "withdrawn".to_owned());
                if p.withdraw {
                    data.insert("ready_for_feedback".into(), false.into());
                }
                let (r, _) = records::update(&tx, &r, &a.name(), records::Patch { state, data, ..Default::default() })?;
                let name = crate::projects::get_by_id(&tx, r.project_id)?.name;
                tx.commit()?;
                Ok((Outcome::new(json!({"id": r.ref_id, "state": r.state, "ready_for_feedback": r.data.get("ready_for_feedback")}))
                    .summary(format!("build {} updated", r.short_id()))
                    .project(r.project.clone())
                    .target(r.ref_id.clone()), newly_ready.then(|| (name, r.clone())), p.withdraw.then_some(r)))
            })
            .await;
        let outcome = outcome.map(|(o, notify, withdrawn)| {
            if let Some((name, r)) = notify {
                crate::web::notify_build(&self.state, &name, &r);
            }
            // A withdrawn build can't be downloaded any more; its archive only takes up space.
            if let Some(r) = withdrawn {
                let dir = crate::builds::dir(&self.state.config.data_dir());
                for path in [
                    crate::builds::archive_path(&dir, r.str("product"), r.str("name")),
                    crate::builds::manifest_path(&dir, r.str("product"), r.str("name")),
                ] {
                    if crate::builds::check_product(r.str("product")).is_ok() && !r.str("name").is_empty() {
                        let _ = std::fs::remove_file(path);
                    }
                }
            }
            o
        });
        self.finish(&actor, "update_build", Action::Write, outcome).await
    }

    #[tool(description = "Take back a button that hasn't run (or failed), e.g. because its command was wrong.")]
    async fn withdraw_button(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ButtonId>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let b = actions::get(c, p.id)?.ok_or_else(|| anyhow::anyhow!("no button {}", p.id))?;
                let r = records::get(c, b.record_id)?;
                writable(c, &a, &r.ref_id)?;
                actions::withdraw(c, b.id, &a.name())?;
                Ok(Outcome::new(json!({"withdrawn": b.id}))
                    .summary(format!("button {} withdrawn", b.id))
                    .project(r.project.clone())
                    .target(r.ref_id))
            })
            .await;
        self.finish(&actor, "withdraw_button", Action::Write, outcome).await
    }
}
