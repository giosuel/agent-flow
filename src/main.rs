//! `agentflow`: the AgentFlow server, a background program started by systemd (`agentflow serve`),
//! with its setup and the helper that works as the agents' user. People and agents type `flow`.

use std::ffi::OsString;
use std::path::PathBuf;

use agentflow::{cli, config, db, github, hands, push, setup, util, web};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "agentflow", version = env!("APP_VERSION"), about = "The AgentFlow server (people and agents use `flow`)")]
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
    },    /// The client commands moved to `flow`. For one release, `flow hello`, `news`,
    /// `publish` and the others are handed to `flow`, so hooks written before keep working.
    #[command(external_subcommand)]
    Moved(Vec<OsString>),
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "agentflow=info,warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let config_path = args.config.clone().unwrap_or_else(config::Config::default_path);
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
        Command::Setup(a) => setup::setup(a),
        Command::Hands { socket, server_user } => hands::serve(&socket, util::uid_of(&server_user)?),
        Command::Moved(rest) => moved(args.config, rest),
    }
}

/// Hands a client command that used to be `agentflow <command>` to `flow`, with the same
/// arguments and environment. Only the commands that moved; anything else is an error as before.
fn moved(config: Option<PathBuf>, rest: Vec<OsString>) -> anyhow::Result<()> {
    let name = rest.first().and_then(|n| n.to_str()).unwrap_or("");
    if !util::MOVED_TO_FLOW.contains(&name) {
        anyhow::bail!("unknown command {name:?} (see agentflow --help; people and agents use `flow`)");
    }
    eprintln!("note: `agentflow {name}` is now `flow {name}`; this forwarding goes away in the next release");
    let mut cmd = std::process::Command::new(util::flow_path());
    if let Some(c) = config {
        cmd.arg("--config").arg(c);
    }
    use std::os::unix::process::CommandExt;
    let err = cmd.args(rest).exec();
    Err(anyhow::anyhow!("starting flow ({}): {err}", util::flow_path().display()))
}
