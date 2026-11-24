//! Tools for the handbook: the owner's rules for every project (see `crate::handbook`). Every agent
//! reads it; the consultant also edits it (the owner, 2026-11-18: "I can edit it and the
//! consultant").

use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData, schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use super::{FlowMcp, Outcome, actor_of};
use crate::activity::Action;
use crate::auth::Actor;
use crate::handbook;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadArg {
    /// The file, e.g. "MACHINE.md" or "templates/external-watch/README.md".
    pub name: String,
    /// Only this section (its heading, e.g. "Several agents at once"); the whole file if left out.
    pub section: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EditArg {
    /// The file, e.g. "OWNER.md"; a new path makes a new file.
    pub name: String,
    /// The version `handbook_read` gave (empty for a new file): an edit over a newer one is refused.
    #[serde(default)]
    pub version: String,
    /// Replace only this section (its heading); the whole file if left out.
    pub section: Option<String>,
    /// The new text (Markdown). For a section, its heading line included. Empty removes the file.
    pub text: String,
    /// What changed, in a few words, for the history.
    pub what: String,
}

#[tool_router(router = handbook_tools, vis = "pub(super)")]
impl FlowMcp {
    #[tool(description = "The owner's handbook (how they work, the machine, development and procedures): its pages \
        with their sections, and its other files. Read the part you need with `handbook_read` instead of whole files. \
        A copy is also on your machine in ~/.local/share/agentflow/handbook.")]
    async fn handbook(&self, Extension(parts): Extension<Parts>) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let outcome = self
            .state
            .db
            .call(|c| {
                let mut pages = Vec::new();
                for name in handbook::pages(c)? {
                    let g = handbook::read(c, &name)?;
                    pages.push(json!({"name": name, "sections": g.sections.iter().map(|s| s.title.clone()).collect::<Vec<_>>(), "size": g.text.len()}));
                }
                let other: Vec<String> = handbook::paths(c)?.into_iter().filter(|p| !handbook::is_page(p)).collect();
                Ok(Outcome::new(json!({"pages": pages, "other_files": other})).summary("handbook"))
            })
            .await;
        self.finish(&actor, "handbook", Action::Read, outcome).await
    }

    #[tool(description = "One handbook file, or one section of it, as Markdown, with its version (for \
        `handbook_edit`). These are the owner's rules: follow them.")]
    async fn handbook_read(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ReadArg>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let outcome = self
            .state
            .db
            .call(move |c| {
                let g = handbook::read(c, p.name.trim())?;
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
                Ok(Outcome::new(
                    json!({"name": g.name, "section": title, "text": text, "version": g.version(), "rev": g.rev}),
                )
                .summary(format!("handbook {} ({title})", g.name)))
            })
            .await;
        self.finish(&actor, "handbook_read", Action::Read, outcome).await
    }

    #[tool(description = "The consultant only: change a handbook file, or one section of it, as a new revision. \
        Give the version `handbook_read` returned; an edit over a newer one is refused (read again and redo it). \
        AgentFlow keeps every revision, and every machine's copy follows. No git, no pull requests.")]
    async fn handbook_edit(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<EditArg>,
    ) -> Result<CallToolResult, ErrorData> {
        let actor = actor_of(&parts);
        let author = actor.name();
        let consultant = matches!(actor, Actor::Admin { .. });
        let data = self.state.config.data_dir();
        let outcome = self
            .state
            .db
            .call(move |c| {
                if !consultant {
                    anyhow::bail!("only the consultant edits the handbook (and the owner, on its page); tell the consultant what to change");
                }
                let name = p.name.trim().to_owned();
                let what = p.what.trim();
                if what.is_empty() {
                    anyhow::bail!("say in a few words what changed (what)");
                }
                let content = match &p.section {
                    Some(s) => {
                        let g = handbook::read(c, &name)?;
                        let i = g.find_section(s).ok_or_else(|| anyhow::anyhow!("{name} has no section {s:?}"))?;
                        handbook::replace_section(&g, i, &p.text)?
                    }
                    None => p.text.clone(),
                };
                let rev = handbook::save(c, &name, p.version.trim(), &content, &author, what).map_err(anyhow::Error::from)?;
                if let Err(e) = handbook::mirror(c, &data) {
                    tracing::warn!("writing out the handbook: {e:#}");
                }
                let version = handbook::read(c, &name).map(|g| g.version()).unwrap_or_default();
                Ok(Outcome::new(json!({"name": name, "rev": rev, "version": version}))
                    .summary(format!("handbook {name}: {what} (rev {rev})")))
            })
            .await;
        self.finish(&actor, "handbook_edit", Action::Write, outcome).await
    }
}
