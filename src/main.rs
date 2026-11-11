//! AgentFlow: one binary with the server (`agentflow serve`), the owner's admin commands, and the
//! client commands agents and the owner run against the server.
#![allow(dead_code, reason = "log kinds and helpers that M3–M8 start using; removed once they all do")]

mod activity;
mod auth;
mod cli;
mod config;
mod db;
mod machine;
mod markdown;
mod mcp;
mod passkey;
mod projects;
mod push;
mod records;
mod tokens;
mod util;
mod waiting;
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
    /// Wait until something happens in your project (the owner answers, presses a button…), print
    /// it and exit. Agents run this in the background, so the session is woken when it ends.
    Wait {
        #[command(flatten)]
        client: cli::ClientArgs,
        /// Only wake for this record, e.g. Q-12.
        #[arg(long)]
        record: Option<String>,
        /// Give up after this many minutes (exit code 2).
        #[arg(long, default_value_t = 480)]
        minutes: u64,
    },
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
        Command::Wait { client, record, minutes } => cli::wait(&config_path, client, record, minutes),
        Command::McpHeaders { token_file } => cli::mcp_headers(&token_file),
    }
}
