//! Tools for reading guides: the owner's rules, from the ai-handbook repository.

use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use super::{FlowMcp, Outcome, actor_of};
use crate::activity::Action;
use crate::guides;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GuideArg {
    /// The file, e.g. "MACHINE.md".
    pub name: String,
    /// Only this section (its heading, e.g. "Several agents at once"); the whole file if left out.
    pub section: Option<String>,
}

#[tool_router(router = guide_tools, vis = "pub(super)")]
impl FlowMcp {
    #[tool(description = "The owner's guides (how they work, the machine, development and procedures), each with its \
        sections. Read the part you need with `guide` instead of loading whole files.")]
    async fn guides(&self, Extension(parts): Extension<Parts>) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let dir = self.state.config.handbook_dir();
        let outcome = tokio::task::spawn_blocking(move || -> anyhow::Result<Outcome> {
            let mut list = Vec::new();
            for name in guides::list(&dir)? {
                let g = guides::read(&dir, &name)?;
                list.push(json!({"name": name, "sections": g.sections.iter().map(|s| s.title.clone()).collect::<Vec<_>>(), "size": g.text.len()}));
            }
            Ok(Outcome::new(json!({"guides": list})).summary("guides"))
        })
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("{e}")));
        self.finish(&actor, "guides", Action::Read, outcome).await
    }

    #[tool(description = "One guide, or one section of it, as Markdown. These are the owner's rules: follow them.")]
    async fn guide(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<GuideArg>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let dir = self.state.config.handbook_dir();
        let outcome = tokio::task::spawn_blocking(move || -> anyhow::Result<Outcome> {
            let g = guides::read(&dir, p.name.trim())?;
            let (title, text) = match &p.section {
                Some(s) => {
                    let i = g.find_section(s).ok_or_else(|| {
                        anyhow::anyhow!(
                            "{} has no section {s:?}; it has: {}",
                            g.name,
                            g.sections.iter().map(|x| x.title.as_str()).collect::<Vec<_>>().join(", ")
                        )
                    })?;
                    (g.sections[i].title.clone(), g.sections[i].text.clone())
                }
                None => ("whole file".into(), g.text.clone()),
            };
            Ok(Outcome::new(json!({"name": g.name, "section": title, "text": text}))
                .summary(format!("guide {} ({title})", g.name)))
        })
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("{e}")));
        self.finish(&actor, "guide", Action::Read, outcome).await
    }
}
