//! `agentflow-builder`: a build machine's background program. It connects to the AgentFlow server
//! and runs the builds and tests agents send with `flow remote` (the owner, 2026-11-13). Its
//! systemd user service starts it; `flow builder add` sets it up.

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
#[command(name = "agentflow-builder", version = env!("APP_VERSION"), about = "AgentFlow's builder: runs builds and tests for agents (flow builder add sets it up)")]
struct Args {
    /// Its config (default ~/.config/agentflow/builder.toml, written by flow builder add).
    #[arg(long, env = "AGENTFLOW_BUILDER_CONFIG")]
    config: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let path = args.config.unwrap_or_else(agentflow::builder::Config::default_path);
    let cfg = agentflow::builder::Config::load(&path)?;
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(agentflow::builder::run(cfg))
}
