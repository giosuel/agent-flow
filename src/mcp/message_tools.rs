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

fn message_json(m: &messages::Message) -> Value {
    json!({"from": m.sender, "kind": m.kind, "at": fmt_utc(m.created_at), "body": m.body, "conversation": m.thread_id})
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

    #[tool(description = "For the consultant: messages to you (or to everyone) you haven't read, oldest first. Marks \
        them read. Agents get their messages through since_last_check and `agentflow wait`.")]
    async fn inbox(&self, Extension(parts): Extension<Parts>) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let a = actor.clone();
        let outcome = self
            .state
            .db
            .call(move |c| {
                if !matches!(a, Actor::Admin { .. }) {
                    anyhow::bail!("inbox is the consultant's; agents use since_last_check");
                }
                let list: Vec<Value> = messages::consultant_inbox(c, true)?.iter().map(message_json).collect();
                Ok(Outcome::new(json!({"messages": list})).summary(format!("inbox: {} messages", list.len())))
            })
            .await;
        self.finish(&actor, "inbox", Action::Read, outcome).await
    }
}
