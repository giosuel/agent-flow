//! The MCP endpoint (`/mcp`, streamable HTTP) the project agents talk to. The HTTP layer has
//! already authenticated the request; each tool reads the identity from the request parts, does
//! its work and writes its own activity-log row.

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
const INSTRUCTIONS: &str = "AgentFlow is the owner's project-management server. Each project has one agent; \
record your project's tasks, decisions, questions for the owner, PRs and builds here instead of in \
Markdown files. Only your own project can be changed with your token; every call is logged.";

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
    #[allow(dead_code, reason = "used by the tool_handler macro")]
    tool_router: ToolRouter<Self>,
}

impl FlowMcp {
    fn new(state: AppState) -> Self {
        Self { state, tool_router: Self::tool_router() }
    }

    async fn log(&self, actor: &Actor, action: Action, summary: String) {
        activity::write(&self.state.db, Entry::new(actor, Via::Mcp, action, summary)).await;
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

#[tool_handler]
impl ServerHandler for FlowMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("agentflow", env!("APP_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}
