//! AgentFlow: one binary with the server (`agentflow serve`), the owner's admin commands, and the
//! client commands agents and the owner run against the server.
#![allow(dead_code, reason = "M0 lays foundations (log fields, permissions) that later milestones use")]

mod activity;
mod auth;
mod cli;
mod config;
mod db;
mod mcp;
mod passkey;
mod projects;
mod push;
mod records;
mod tokens;
mod util;
mod web;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "agentflow", version = env!("APP_VERSION"), about = "Project management for Claude agents and their owner")]
struct Args {
    /// The server's config file.
    #[arg(long, global = true, env = "AGENTFLOW_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server.
    Serve,
    /// Work on the server's database directly (run on the server, as its user).
    Admin {
        #[command(subcommand)]
        command: cli::AdminCommand,
    },
    /// Ask the server who your token belongs to.
    Whoami(cli::ClientArgs),
    /// Print the Authorization header for Claude Code's `headersHelper`, from a token file.
    McpHeaders {
        /// File holding the token, e.g. ~/.config/agentflow/tokens/<project>.token
        #[arg(long)]
        token_file: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "agentflow=info,warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let config_path = args.config.unwrap_or_else(config::Config::default_path);
    match args.command {
        Command::Serve => {
            let config = config::Config::load(&config_path)?;
            let db = db::Db::open(&config.db_path())?;
            let vapid = push::Vapid::load_or_create(&config.data_dir())?;
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            runtime.block_on(web::serve(web::AppState::new(db, config, vapid)))
        }
        Command::Admin { command } => cli::admin(&config_path, command),
        Command::Whoami(client) => cli::whoami(&config_path, client),
        Command::McpHeaders { token_file } => cli::mcp_headers(&token_file),
    }
}
