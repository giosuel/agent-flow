//! AgentFlow: one binary with the server (`agentflow serve`), the owner's admin commands, and the
//! client commands agents and the owner run against the server.

mod actions;
mod activity;
mod auth;
mod backup;
mod builds;
mod cli;
mod config;
mod contributions;
mod db;
mod demo;
mod github;
mod guides;
mod hands;
mod hardening;
mod logview;
mod machine;
mod markdown;
mod mcp;
mod messages;
mod onboard;
mod outgoing;
mod passkey;
mod poller;
mod projectdoc;
mod projects;
mod prs;
mod push;
mod records;
mod remote;
mod screenshots;
mod setup;
mod steps;
mod times;
mod tokens;
mod totest;
mod tz;
mod usage;
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
    /// Publish a test build for the owner (agents; like viber-run's publish.sh).
    Publish(cli::PublishArgs),
    /// Upload a screenshot to your project's Screenshots (agents): a test build, a mockup, a
    /// spike test, a bug. Name and category required; a description and a record optional.
    Screenshot(cli::ScreenshotArgs),
    /// Draft a PR, issue, comment or new commits for someone else's repository (agents). The
    /// owner reads it in AgentFlow and sends it, as them; agents never post or push there.
    Contribute(cli::ContributeArgs),
    /// Run a build or test on a build server (agents): `agentflow remote -- cargo build --release`.
    /// Copies only what changed, prints the output as it comes, exits with the command's exit code
    /// (75: the server is offline, build here instead).
    Remote(cli::RemoteArgs),
    /// Run a build or test and log how long it took (agents): `agentflow time --on T-12 -- cargo
    /// test --workspace`. Measures wall time, CPU and peak memory, prints them, logs them on the
    /// task, step or PR, and exits with the command's exit code.
    Time(cli::TimeArgs),
    /// Fetch and start a test build (the owner's machines). Also `flow-run`: the same program
    /// called by that name runs this. Without a product, lists the products; `run <product>
    /// list` lists its builds.
    Run(cli::RunArgs),
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
    /// Make AgentFlow a system service running as its own user (run once with sudo after
    /// installing; `--dry-run` shows the steps).
    Setup(setup::SetupArgs),
    /// Do the server's jobs as the agents' user (run as that user, when AgentFlow runs as its own):
    /// the owner's buttons, gh, guide edits, token files, new projects' folders, transcripts.
    Hands {
        /// Where to listen.
        #[arg(long, default_value = "/run/agentflow-hands/hands.sock")]
        socket: PathBuf,
        /// The only user allowed to send jobs: AgentFlow's own.
        #[arg(long, default_value = "agentflow")]
        server_user: String,
    },
    /// Print the Authorization header for Claude Code's `headersHelper`, from a token file.
    McpHeaders {
        /// File holding the token, e.g. ~/.config/agentflow/tokens/<project>.token
        #[arg(long)]
        token_file: PathBuf,
    },
}

/// Called as `flow-run` (a link to this program), it's `agentflow run`: the owner's command for
/// test builds (the owner, 2026-11-11: "lets call the client tool flow-run").
fn as_flow_run(mut argv: Vec<std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let called = argv.first().and_then(|a| std::path::Path::new(a).file_name().map(|n| n.to_owned()));
    if called.as_deref() == Some(std::ffi::OsStr::new("flow-run")) {
        argv[0] = "agentflow".into();
        argv.insert(1, "run".into());
    }
    argv
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse_from(as_flow_run(std::env::args_os().collect()));
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
            config.apply();
            let db = db::Db::open(&config.db_path())?;
            let vapid = push::Vapid::load_or_create(&config.data_dir())?;
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
            runtime.block_on(web::serve(web::AppState::new(db, config, vapid, std::sync::Arc::new(github::GhCli))))
        }
        Command::Admin { command } => cli::admin(&config_path, command),
        Command::Whoami(client) => cli::whoami(&config_path, client),
        Command::Publish(args) => cli::publish(&config_path, args),
        Command::Screenshot(args) => cli::screenshot(&config_path, args),
        Command::Contribute(args) => cli::contribute(&config_path, args),
        Command::Remote(args) => {
            let code = cli::remote(&config_path, args)?;
            std::process::exit(code);
        }
        Command::Time(args) => {
            let code = cli::time(&config_path, args)?;
            std::process::exit(code);
        }
        Command::Run(args) => cli::run(&config_path, args),
        Command::Wait { client, record, minutes } => cli::wait(&config_path, client, record, minutes),
        Command::McpHeaders { token_file } => cli::mcp_headers(&token_file),
        Command::Setup(a) => setup::setup(a),
        Command::Hands { socket, server_user } => hands::serve(&socket, util::uid_of(&server_user)?),
    }
}

#[cfg(test)]
mod flow_run_tests {
    #[test]
    fn called_as_flow_run_it_runs_builds() {
        let args = |v: &[&str]| v.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
        assert_eq!(
            super::as_flow_run(args(&["/home/x/.local/bin/flow-run", "lumen", "2"])),
            args(&["agentflow", "run", "lumen", "2"])
        );
        assert_eq!(super::as_flow_run(args(&["flow-run"])), args(&["agentflow", "run"]));
        assert_eq!(super::as_flow_run(args(&["agentflow", "serve"])), args(&["agentflow", "serve"]));
    }
}
