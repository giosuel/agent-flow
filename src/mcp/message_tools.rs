//! Tools for contributions to other people's repositories: drafts the owner sends, and watching them.
//! (Agents talk to each other with Claude Code's SendMessage; AgentFlow logs it: crate::sessionlog.)

use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use super::{FlowMcp, Outcome, actor_of};
use crate::activity::Action;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TrackContribution {
    /// The project it came from (the consultant names it; an agent's own if left out).
    pub project: Option<String>,
    /// The other repository, e.g. "loro-dev/loro".
    pub repo: String,
    /// The PR's or issue's number there.
    pub number: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DraftContribution {
    /// "issue" (a new issue on their repository) or "comment" (on a PR or issue already sent).
    /// A PR or new commits need your commits: use the `flow contribute pr|commits` command.
    #[serde(rename = "type")]
    pub kind: String,
    /// Their repository, e.g. "ophymx/formulary" (issue).
    #[serde(default)]
    pub repo: String,
    /// The issue's title.
    #[serde(default)]
    pub title: String,
    /// The text for them (Markdown), as it should appear there.
    pub body: String,
    /// The contribution a comment is for, e.g. "C-4".
    #[serde(default)]
    pub on: String,
    /// A new version of this draft after the owner asked for changes, e.g. "C-7".
    #[serde(default)]
    pub revise: String,
    /// For the owner only: why, in a sentence or two.
    #[serde(default)]
    pub why: String,
    /// Your task it's for, e.g. "T-88".
    #[serde(default)]
    pub task: String,
    /// The owner's decision that agreed to it, e.g. "D-31".
    #[serde(default)]
    pub agreed: String,
    /// The project it's for (the consultant names it; an agent's own if left out).
    pub project: Option<String>,
}

#[tool_router(router = message_tools, vis = "pub(super)")]
impl FlowMcp {
    #[tool(description = "Draft an issue for someone else's repository, or a comment on a PR or issue already sent. \
        Never post or push to other people's repositories yourself: the owner reads the draft in AgentFlow, may edit \
        it, and presses Send; it's then posted as them. Their Send, \"Ask for changes\" (with a note) or Drop comes \
        back to you as news. For a PR or new commits, use the `flow contribute pr|commits` command.")]
    async fn draft_contribution(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<DraftContribution>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = async {
            if !matches!(p.kind.trim(), "issue" | "comment") {
                anyhow::bail!(
                    "this tool drafts an issue or a comment; a PR or new commits go through `flow contribute`"
                );
            }
            let d = crate::outgoing::DraftIn {
                kind: p.kind,
                repo: p.repo,
                title: p.title,
                body: p.body,
                on: p.on,
                revise: p.revise,
                why: p.why,
                task: p.task,
                agreed: p.agreed,
                project: p.project,
                ..Default::default()
            };
            let (data_dir, git_host) = (self.state.config.data_dir(), self.state.config.git_host.clone());
            let env = crate::outgoing::Env { data_dir: &data_dir, git_host: &git_host };
            let r = crate::outgoing::submit(&self.state.db, &env, &a, d).await?;
            let version = crate::web::outgoing::tell_owner(&self.state, &r);
            Ok(Outcome::new(
                json!({"id": r.ref_id, "version": version, "url": r.url(), "state": "waiting for the owner"}),
            )
            .summary(format!("drafted {}: {}", r.short_id(), r.title))
            .project(r.project.clone())
            .target(r.ref_id))
        }
        .await;
        self.finish(&actor, "draft_contribution", Action::Write, outcome).await
    }

    #[tool(
        description = "Watch a PR or issue that was sent to someone else's repository before AgentFlow sent them (only \
        after the owner agreed to it): AgentFlow checks it every 30 minutes and tells the project's agent about new \
        comments, reviews, a merge or a close. Ones the owner sends from a draft are watched already."
    )]
    async fn track_contribution(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<TrackContribution>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = async {
            crate::github::check_repo(p.repo.trim())?;
            let (a2, proj) = (a.clone(), p.project.clone());
            let project = self
                .state
                .db
                .call(move |c| {
                    let p = super::target_project(c, &a2, proj.as_deref(), true)?;
                    crate::contributions::check_room(c, p.id)?;
                    Ok(p)
                })
                .await?;
            let info = self.state.github.item(p.repo.trim(), p.number).await?;
            let (repo, number, author) = (p.repo.trim().to_owned(), p.number, a.name());
            let r = self
                .state
                .db
                .call(move |c| crate::contributions::track(c, project.id, &repo, number, &info, &author))
                .await?;
            Ok(Outcome::new(json!({"id": r.ref_id, "state": r.state, "url": r.str("url")}))
                .summary(format!("watching {}", r.title))
                .project(r.project.clone())
                .target(r.ref_id))
        }
        .await;
        self.finish(&actor, "track_contribution", Action::Write, outcome).await
    }
}
