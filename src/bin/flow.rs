//! `flow`: the one command people and agents type. It talks to an AgentFlow server over its API
//! (the owner, 2026-11-13: "lets use flow"; "can we merge this all into flow?"). The server is the
//! background program `agentflow`.

use std::path::PathBuf;

use agentflow::cli;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "flow", version = env!("APP_VERSION"), about = "AgentFlow: test builds, projects and the agents' commands")]
struct Args {
    /// A file with the server's address (public_url), instead of ~/.config/agentflow/config.toml.
    #[arg(long, global = true, env = "AGENTFLOW_CONFIG", hide = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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
    /// Run a build or test on a build server (agents): `flow remote -- cargo build --release`.
    /// Copies only what changed, prints the output as it comes, exits with the command's exit code
    /// (75: the server is offline, build here instead).
    Remote(cli::RemoteArgs),
    /// Run a build or test and log how long it took (agents): `flow time --on T-12 -- cargo
    /// test --workspace`. Measures wall time, CPU and peak memory, prints them, logs them on the
    /// task, step or PR, and exits with the command's exit code.
    Time(cli::TimeArgs),
    /// Hand in a file as a note (agents): a morning report, a plan, an estimate, a benchmark, a
    /// log. `flow note report.md --name "Morning report, 12 Nov" --category "morning report"`.
    Note(cli::NoteArgs),
    /// Fetch and start a test build (the owner's machines). Without a product, lists the
    /// products; `run <product> list` lists its builds.
    Run(cli::RunArgs),
    /// Print the hello tool's reply as text (your project, its document, milestones, where you are,
    /// questions, notes, news), for a Claude Code SessionStart hook. It reads the hook's input on
    /// stdin when there is one. News in it is marked read.
    Hello {
        #[command(flatten)]
        client: cli::ClientArgs,
        /// Your session's name (default: the last one given).
        #[arg(long, default_value = "")]
        session: String,
        /// The machine (default: the last one given).
        #[arg(long, default_value = "")]
        machine: String,
        /// Your model (default: from the hook's input, or the last one given).
        #[arg(long, default_value = "")]
        model: String,
        /// Your effort level (default: the last one given).
        #[arg(long, default_value = "")]
        effort: String,
        /// Only the project document (no side effects): one of the two hooks, since Claude Code
        /// caps a hook's output at 10,000 characters.
        #[arg(long, conflicts_with = "no_doc")]
        doc: bool,
        /// Everything but the project document, shortened to fit a hook: the other hook.
        #[arg(long)]
        no_doc: bool,
    },
    /// What's new from the owner and others since the agent last read its news (marked read);
    /// prints nothing when nothing is. For a Claude Code hook before each prompt.
    News {
        #[command(flatten)]
        client: cli::ClientArgs,
        /// Show the last stretch again, read or not, e.g. "1h" or "30 min".
        #[arg(long)]
        again: Option<String>,
    },
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
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .with_writer(std::io::stderr)
        .init();
    let config_path = args.config.unwrap_or_else(agentflow::config::Config::default_path);
    match args.command {
        Command::Whoami(client) => cli::whoami(&config_path, client),
        Command::Publish(args) => cli::publish(&config_path, args),
        Command::Screenshot(args) => cli::screenshot(&config_path, args),
        Command::Contribute(args) => cli::contribute(&config_path, args),
        Command::Remote(args) => {
            let code = cli::remote(&config_path, args)?;
            std::process::exit(code);
        }
        Command::Note(args) => cli::note(&config_path, args),
        Command::Time(args) => {
            let code = cli::time(&config_path, args)?;
            std::process::exit(code);
        }
        Command::Run(args) => cli::run(&config_path, args),
        Command::Hello { client, session, machine, model, effort, doc, no_doc } => {
            let part = if doc { cli::HelloPart::Doc } else if no_doc { cli::HelloPart::NoDoc } else { cli::HelloPart::All };
            cli::hello(&config_path, client, cli::HelloArgs { session, machine, model, effort, part })
        }
        Command::News { client, again } => cli::news(&config_path, client, again),
        Command::Wait { client, record, minutes } => cli::wait(&config_path, client, record, minutes),
        Command::McpHeaders { token_file } => cli::mcp_headers(&token_file),
    }
}
