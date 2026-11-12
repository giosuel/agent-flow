//! The MCP endpoint (`/mcp`, streamable HTTP) the project agents talk to. The HTTP layer has
//! already authenticated the request; each tool reads the identity from the request parts, does
//! its work and writes its own activity-log row.

mod build_tools;
mod guide_tools;
mod message_tools;
mod pr_tools;
mod project_tools;
pub(crate) use project_tools::{Hello, hello_reply};
mod record_tools;

use std::sync::Arc;

use axum::http::request::Parts;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::Extension;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use serde_json::json;

use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::web::AppState;

/// What every agent sees when it connects. The full guides come later through `guide()`.
const INSTRUCTIONS: &str = "AgentFlow is the owner's project-management server. Each project has one agent \
(you, for your project); it records the project's milestones, steps, tasks, decisions, ADRs, questions for the \
owner, notes, PRs and builds here instead of in Markdown files, and only it writes here (subagents report back \
to it). Start every session with `hello`: it returns your project document (how to work on the project, its \
stack, its roadmap if it has one; keep it current with `edit_project_doc`), `where` (the steps under way, their \
tasks in doing, the next one, what's blocked) and the notes coming back to you; after your context is reset or \
summarized, call `hello` again with `brief: true`. Work in steps: add a step's tasks \
in one `add_tasks` call, each with an estimate; set a task to doing when you start it (that starts the step) and \
done when it is; `conclude_step` once its tasks are done, with a few lines. Your open PRs never hold a step: stack \
them and move on; every PR gets its Merge button and every release its Push tag button by themselves (a PR \
that isn't a draft is ready), and pushing a release's tag closes its milestone. Time is \
counted from the tasks' states; log builds and tests with `agentflow time [--on T-12] -- <command>`. Ask the \
owner with `ask_question`, one question per call, with numbered options and your recommendation, and repeat the \
same questions with their ids in your chat reply (`in_chat` when the owner is talking with you right now: no \
notification); `blocks` names the tasks that wait for the answer. A question has a scope (a milestone, by default \
the current one, or \"project\"), and its answer becomes a decision there by itself (one given in chat: \
`answer_question`); then check whether it changes an earlier decision and edit, archive, re-scope or replace that \
one (`update_decision`). Record other things the owner decides with `record_decision`, quoting their words, in \
the milestone they belong to or \"project\" for what holds across it. You see project-wide decisions and those of \
milestones that aren't closed; `find_records` reaches the rest with include_closed. Milestones, steps, tasks, \
decisions and questions are yours to add, edit and archive freely (nothing is deleted; include_archived finds \
archived ones). The owner's answers and other news come in the replies of your write calls and with \
`since_last_check`. Show the \
owner what you see with screenshots: `agentflow screenshot <file> --name \"…\" --category \"…\"` (mockups with \
--design-url); hand in files (reports, plans, logs) with `agentflow note <file> --name \"…\" --category \"…\"` and \
short notes or known issues with `add_note`. Never post or push to other people's repositories yourself (PRs, \
issues, comments, commits), even if a token would let you: draft them with `agentflow contribute pr|commits` or \
`draft_contribution` (issue, comment), and the owner sends them from AgentFlow. When something is ready for the \
owner to try, add it with `add_to_test` (a category of your choosing); open items come along with every release \
and build until the owner tested them, and you mark what they tell you with `resolve_tests`. For heavy builds and \
tests, use the build server when it's online: `agentflow remote [--fetch target/release/app] -- cargo build \
--release` runs it there and prints the output; exit code 75 means it's offline, so build here. Your token can \
change only your own project and read all others. Every call is logged.";

pub fn service(state: AppState) -> StreamableHttpService<FlowMcp, LocalSessionManager> {
    let host = state.config.public_host().to_owned();
    let listen = state.config.listen.to_string();
    let bare_host = host.split(':').next().unwrap_or(&host).to_owned();
    let config = StreamableHttpServerConfig::default()
        // DNS-rebinding protection: only our own names. Authentication is the real gate.
        .with_allowed_hosts([host, bare_host, listen, "localhost".into(), "127.0.0.1".into()]);
    StreamableHttpService::new(
        move || Ok(FlowMcp::new(state.clone())),
        Arc::new(LocalSessionManager::default()),
        config,
    )
}

#[derive(Clone)]
pub struct FlowMcp {
    state: AppState,
    tool_router: ToolRouter<Self>,
}

impl FlowMcp {
    fn new(state: AppState) -> Self {
        Self {
            state,
            tool_router: Self::tool_router()
                + Self::project_tools()
                + Self::record_tools()
                + Self::pr_tools()
                + Self::build_tools()
                + Self::message_tools()
                + Self::guide_tools(),
        }
    }

    async fn log(&self, actor: &Actor, action: Action, summary: String) {
        activity::write(&self.state.db, Entry::new(actor, Via::Mcp, action, summary)).await;
    }

    /// Turns a tool's outcome into its reply and its activity-log row. Errors go back to the agent
    /// as a tool error it can read and fix, not as a protocol error.
    async fn finish(
        &self,
        actor: &Actor,
        tool: &str,
        action: Action,
        outcome: anyhow::Result<Outcome>,
    ) -> Result<CallToolResult, ErrorData> {
        // Every reply says when something is waiting to be read, so a busy session notices a
        // message without having to ask. (since_last_check is the reading itself.)
        let a = actor.clone();
        // A write call's reply carries the news itself (marked read), so a busy agent hears the
        // owner's answers at its next call without asking; other replies say how much is waiting.
        let carry = action == Action::Write && outcome.is_ok() && matches!(actor, Actor::Project { .. });
        let (unread, news) = self
            .state
            .db
            .call(move |c| match &a {
                Actor::Project { project_id, slug, .. } => {
                    // A sign of life; the first call of a new agent also starts its read cursor.
                    crate::projects::seen(c, *project_id)?;
                    if carry {
                        let news = crate::waiting::read_news(c, *project_id, slug)?;
                        Ok((0, news.iter().map(crate::waiting::news_json).collect::<Vec<_>>()))
                    } else {
                        Ok((crate::waiting::unread_count(c, *project_id, slug)?, Vec::new()))
                    }
                }
                _ => Ok((0, Vec::new())),
            })
            .await
            .unwrap_or((0, Vec::new()));
        match outcome {
            Ok(o) => {
                let mut e = Entry::new(actor, Via::Mcp, action, o.summary.unwrap_or_else(|| tool.to_owned()));
                if let Some(p) = o.project {
                    e = e.project(p);
                }
                if let Some(t) = o.target {
                    e = e.target(t);
                }
                activity::write(&self.state.db, e).await;
                let mut reply = o.reply;
                if !news.is_empty()
                    && let Some(obj) = reply.as_object_mut()
                {
                    obj.insert("news".into(), serde_json::Value::Array(news));
                }
                if unread > 0
                    && !matches!(tool, "since_last_check" | "hello")
                    && let Some(obj) = reply.as_object_mut()
                {
                    obj.insert("unread".into(), serde_json::json!(format!("{unread} new for you; call since_last_check")));
                }
                Ok(json_result(reply))
            }
            Err(err) => {
                let msg = format!("{err:#}");
                let e = Entry::new(actor, Via::Mcp, action, format!("{tool} refused: {msg}")).severity("warn");
                activity::write(&self.state.db, e).await;
                Ok(error_result(msg))
            }
        }
    }
}

/// What a tool did: its reply, and what the log should say about it.
pub struct Outcome {
    pub reply: serde_json::Value,
    pub summary: Option<String>,
    pub project: Option<String>,
    pub target: Option<String>,
}

impl Outcome {
    pub fn new(reply: serde_json::Value) -> Self {
        Self { reply, summary: None, project: None, target: None }
    }

    pub fn summary(mut self, s: impl Into<String>) -> Self {
        self.summary = Some(s.into());
        self
    }

    pub fn project(mut self, p: impl Into<String>) -> Self {
        self.project = Some(p.into());
        self
    }

    pub fn target(mut self, t: impl Into<String>) -> Self {
        self.target = Some(t.into());
        self
    }
}

/// A record a tool may change: in the caller's own project (any, for the consultant).
pub fn writable(c: &rusqlite::Connection, actor: &Actor, id: &str) -> anyhow::Result<crate::records::Record> {
    let r = crate::records::find_str(c, id, actor.project())?;
    if !actor.can_write_project(r.project_id) {
        anyhow::bail!("your token can change only your own project's records; {} belongs to {}", r.ref_id, r.project);
    }
    Ok(r)
}

/// The project a tool acts on. A project token acts on its own project and may read others; the
/// consultant (admin) names the project; the owner's devices only read.
pub fn target_project(
    conn: &rusqlite::Connection,
    actor: &Actor,
    project: Option<&str>,
    write: bool,
) -> anyhow::Result<crate::projects::Project> {
    let project = project.map(str::trim).filter(|p| !p.is_empty());
    match actor {
        Actor::Project { slug, .. } => match project {
            Some(p) if p != slug && write => {
                anyhow::bail!("your token can change only {slug}; ask {p}'s agent or the consultant")
            }
            Some(p) => crate::projects::require(conn, p),
            None => crate::projects::require(conn, slug),
        },
        Actor::Admin { .. } | Actor::System(_) => {
            crate::projects::require(conn, project.ok_or_else(|| anyhow::anyhow!("say which project"))?)
        }
        Actor::Device { .. } | Actor::Owner { .. } => {
            if write {
                anyhow::bail!("this token can only read");
            }
            crate::projects::require(conn, project.ok_or_else(|| anyhow::anyhow!("say which project"))?)
        }
        Actor::Anonymous => anyhow::bail!("not signed in"),
    }
}

fn actor_of(parts: &Parts) -> Actor {
    parts.extensions.get::<Actor>().cloned().unwrap_or(Actor::Anonymous)
}

pub fn json_result(value: serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(serde_json::to_string_pretty(&value).unwrap_or_default())])
}

pub fn error_result(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

#[tool_router]
impl FlowMcp {
    #[tool(
        description = "Who AgentFlow thinks you are: your project, your token's kind, and the server version. Call it once to check the connection."
    )]
    async fn whoami(&self, Extension(parts): Extension<Parts>) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        self.log(&actor, Action::Read, "whoami".into()).await;
        Ok(json_result(json!({
            "kind": actor.kind(),
            "name": actor.name(),
            "project": actor.project(),
            "version": env!("APP_VERSION"),
        })))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for FlowMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("agentflow", env!("APP_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}
