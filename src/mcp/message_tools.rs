//! Tools for conversations: writing to other agents, the consultant or everyone, and reading.

use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{FlowMcp, Outcome, actor_of};
use crate::activity::Action;
use crate::auth::Actor;
use crate::messages::{self, CONSULTANT};
use crate::util::fmt_utc;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendMessage {
    /// Who it's for: project slugs (e.g. ["antimony"]), "consultant", or "all" for every project.
    /// When replying in a conversation, leave it empty (or add someone new).
    #[serde(default)]
    pub to: Vec<String>,
    /// The subject of a new conversation, e.g. "Port 21300 for the demo server?".
    #[serde(default)]
    pub subject: String,
    pub body: String,
    /// info, question, answer, resource (CPU, memory, ports, heavy jobs) or handover.
    pub kind: String,
    /// Reply in this conversation (its number) instead of starting one.
    pub conversation: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConversationId {
    /// The conversation's number.
    pub conversation: i64,
}

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
    /// A PR or new commits need your commits: use the `agentflow contribute pr|commits` command.
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListConversations {
    /// At most this many, newest activity first (default 20).
    pub limit: Option<i64>,
}

/// How an actor appears in conversations.
fn who(actor: &Actor) -> anyhow::Result<String> {
    Ok(match actor {
        Actor::Project { slug, .. } => slug.clone(),
        Actor::Admin { .. } => CONSULTANT.into(),
        _ => anyhow::bail!("only project agents and the consultant write here"),
    })
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct InboxArgs {
    /// The id of the last message you handled: it and everything before it are marked read.
    pub read_up_to: Option<i64>,
}

fn message_json(m: &messages::Message) -> Value {
    json!({"id": m.id, "from": m.sender, "kind": m.kind, "at": fmt_utc(m.created_at), "body": m.body, "conversation": m.thread_id})
}

#[tool_router(router = message_tools, vis = "pub(super)")]
impl FlowMcp {
    #[tool(description = "Write to another project's agent, the consultant, or every project (\"all\"): coordinate \
        resources (heavy builds, ports), ask how something is done, hand something over. Starts a conversation or replies \
        in one. Use this instead of Claude Code's SendMessage: everything here is kept and the owner can read it.")]
    async fn send_message(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<SendMessage>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let me = who(&a)?;
                let tx = c.transaction()?;
                let (thread, id) = messages::send(&tx, &me, &p.to, &p.subject, &p.body, p.kind.trim(), p.conversation)?;
                tx.commit()?;
                let mut o = Outcome::new(json!({"conversation": thread, "message": id}))
                    .summary(format!("message in conversation {thread}"));
                if let Actor::Project { slug, .. } = &a {
                    o = o.project(slug.clone());
                }
                Ok(o)
            })
            .await;
        self.finish(&actor, "send_message", Action::Message, outcome).await
    }

    #[tool(description = "One conversation in full: who's in it and every message.")]
    async fn conversation(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ConversationId>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let me = who(&a)?;
                let t = messages::thread(c, p.conversation)?
                    .ok_or_else(|| anyhow::anyhow!("no conversation {}", p.conversation))?;
                let visible = me == CONSULTANT || t.members.iter().any(|m| *m == me || m == messages::ALL);
                if !visible {
                    anyhow::bail!("you aren't part of conversation {}", p.conversation);
                }
                let msgs: Vec<Value> = messages::messages(c, t.id)?.iter().map(message_json).collect();
                Ok(Outcome::new(
                    json!({"conversation": t.id, "subject": t.subject, "members": t.members, "messages": msgs}),
                )
                .summary(format!("read conversation {}", t.id)))
            })
            .await;
        self.finish(&actor, "conversation", Action::Read, outcome).await
    }

    #[tool(description = "Your conversations, newest activity first (the consultant sees all of them).")]
    async fn conversations(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ListConversations>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                let me = who(&a)?;
                let member = (me != CONSULTANT).then_some(me.as_str());
                let list: Vec<Value> = messages::threads(c, member, p.limit.unwrap_or(20).clamp(1, 200), None)?
                    .iter()
                    .map(|t| json!({"conversation": t.id, "subject": t.subject, "members": t.members, "messages": t.count, "last": fmt_utc(t.last_at)}))
                    .collect();
                Ok(Outcome::new(json!({"conversations": list})).summary("conversations"))
            })
            .await;
        self.finish(&actor, "conversations", Action::Read, outcome).await
    }

    #[tool(description = "Draft an issue for someone else's repository, or a comment on a PR or issue already sent. \
        Never post or push to other people's repositories yourself: the owner reads the draft in AgentFlow, may edit \
        it, and presses Send; it's then posted as them. Their Send, \"Ask for changes\" (with a note) or Drop comes \
        back to you as news. For a PR or new commits, use the `agentflow contribute pr|commits` command.")]
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
                    "this tool drafts an issue or a comment; a PR or new commits go through `agentflow contribute`"
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

    #[tool(
        description = "For the consultant: messages to you (or to everyone) you haven't marked read, oldest first. \
        Once you've handled some, pass the last one's id as read_up_to: it and everything before it are marked read, \
        and the rest come back. Until then they keep coming (and `agentflow wait` keeps waking you). Agents get their \
        messages through since_last_check and `agentflow wait`."
    )]
    async fn inbox(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<InboxArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                if !matches!(a, Actor::Admin { .. }) {
                    anyhow::bail!("inbox is the consultant's; agents use since_last_check");
                }
                if let Some(id) = p.read_up_to {
                    messages::mark_consultant_read(c, id)?;
                }
                let list: Vec<Value> = messages::consultant_inbox(c)?.iter().map(message_json).collect();
                Ok(Outcome::new(json!({"messages": list})).summary(format!("inbox: {} messages", list.len())))
            })
            .await;
        self.finish(&actor, "inbox", Action::Read, outcome).await
    }
}
