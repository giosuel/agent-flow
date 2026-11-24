//! Tools for test builds: the agent's view of what it published and what the owner said.

use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::pr_tools::ProjectArg;
use super::{FlowMcp, Outcome, actor_of, target_project, writable};
use crate::activity::Action;
use crate::builds;
use crate::records::{self, Kind};

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

#[tool_router(router = build_tools, vis = "pub(super)")]
impl FlowMcp {
    #[tool(description = "Your project's test builds, newest first: number, name, what's in it, whether the owner \
        tried it, and their feedback. Publish new ones with `flow publish` (a command, not a tool).")]
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
                    let list: Vec<Value> = fs.iter().filter_map(|f| builds::feature(f)).collect();
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
}
