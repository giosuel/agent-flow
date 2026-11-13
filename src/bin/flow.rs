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
    /// Connect this machine to an AgentFlow server: you approve it in the browser (it opens when
    /// it can; otherwise open the link it shows, e.g. on your phone).
    Connect {
        /// The server, e.g. https://flow.example
        url: String,
    },
    /// Disconnect this machine: its token is revoked on the server and removed here.
    Disconnect,
    /// The server this machine uses, whether it's reachable, both versions and who you are.
    Status(cli::ClientArgs),
    /// Replace this flow with the one the server hands out.
    Update(cli::ClientArgs),
    /// Tokens: create (projects, the consultant), list, revoke, rotate. Changes need your approval
    /// in the browser.
    Token {
        #[command(subcommand)]
        command: TokenCommand,
        #[command(flatten)]
        client: cli::ClientArgs,
    },
    /// Projects: add (with your approval in the browser), list.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
        #[command(flatten)]
        client: cli::ClientArgs,
    },
    /// Back up the server's database now and export every project (with your approval).
    Backup(cli::ClientArgs),
    /// Bring a project's older PRs in as records, e.g. `flow import-prs imaginarium 300-374`
    /// (with your approval).
    ImportPrs {
        project: String,
        /// The PR numbers, e.g. 300-374.
        range: String,
        #[command(flatten)]
        client: cli::ClientArgs,
    },
    /// Build machines: add this one (with your approval), list, edit, remove.
    Builder {
        #[command(subcommand)]
        command: BuilderCommand,
        #[command(flatten)]
        client: cli::ClientArgs,
    },
    /// On the server itself: `sudo flow server add-passkey` makes a one-time link to add a passkey,
    /// when you've lost yours or have none yet.
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    /// Who your token belongs to (now part of `flow status`).
    #[command(hide = true)]
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
    /// Send what's new in this project's Claude conversations to AgentFlow (its hooks run this
    /// after each reply, subagent and session end). `--all`: every earlier conversation of the folder.
    Sync {
        #[command(flatten)]
        client: cli::ClientArgs,
        #[arg(long)]
        all: bool,
        /// The files to send (the detached copy a hook starts).
        #[arg(long = "file", hide = true)]
        files: Vec<PathBuf>,
    },
    /// Print the Authorization header for Claude Code's `headersHelper`, from a token file.
    McpHeaders {
        /// File holding the token, e.g. ~/.config/agentflow/tokens/<project>.token
        #[arg(long)]
        token_file: PathBuf,
    },
}

#[derive(Subcommand)]
enum TokenCommand {
    /// Create a token: `project <project>` for a project's agent, or `consultant`.
    Create {
        /// project or consultant
        kind: String,
        /// The project, for a project token.
        project: Option<String>,
        /// What it's for; for a project token, "<project> agent" if left out.
        #[arg(long)]
        name: Option<String>,
        /// Write it to this file (mode 600) instead of printing it.
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Every active token, without its secret.
    List,
    /// Revoke a token by its id (from `flow token list`).
    Revoke { id: i64 },
    /// Replace a token with a new one and revoke the old.
    Rotate {
        id: i64,
        /// Write the new one to this file (mode 600) instead of printing it.
        #[arg(long)]
        file: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ProjectCommand {
    /// Set a project up in this folder (or the one given): with the setup code from its page, or,
    /// pressing Enter at the prompt, as a new project (with your approval in the browser).
    Setup {
        /// The folder, if not this one; made if it doesn't exist.
        folder: Option<PathBuf>,
        /// The setup code from the project's page (asked for if left out).
        #[arg(long)]
        code: Option<String>,
        /// A new project's name (asked for if left out).
        #[arg(long)]
        name: Option<String>,
    },
    /// Add a project (its folder and agent come with `flow project setup` there).
    Add {
        /// Short name used in record ids and URLs, e.g. "speculum".
        slug: String,
        /// Display name, e.g. "Speculum".
        #[arg(long)]
        name: String,
    },
    /// Every project with its folder.
    List,
}

#[derive(Subcommand)]
enum BuilderCommand {
    /// Make this machine a builder: asks whether it gets a user of its own (recommended), its
    /// name, and your approval in the browser; then starts its service.
    Add {
        /// Its name (default: this machine's hostname; asked for when there's a terminal).
        #[arg(long)]
        name: Option<String>,
        /// The user it runs as, made if it doesn't exist (default: asked; agentflow-builder).
        #[arg(long, conflicts_with = "current_user")]
        user: Option<String>,
        /// Run it as you instead of a user of its own.
        #[arg(long)]
        current_user: bool,
    },
    /// All builders: online or not, jobs, limits.
    List,
    /// Change a builder's name or limits (this machine's if no name is given).
    Edit {
        name: Option<String>,
        /// A new name.
        #[arg(long = "name")]
        rename: Option<String>,
        /// Start another job only while at least this many threads are free…
        #[arg(long)]
        min_threads: Option<f64>,
        /// …and this much memory (GB).
        #[arg(long)]
        min_memory: Option<f64>,
        /// Pause jobs below this much free memory (GB).
        #[arg(long)]
        reserve_memory: Option<f64>,
        /// Delete a project's work folder unused for this many days.
        #[arg(long)]
        keep_days: Option<i64>,
    },
    /// Remove a builder: on the builder itself, its service, token and (if you say so) folders;
    /// from elsewhere, with your approval.
    Remove { name: Option<String> },
}

#[derive(Subcommand)]
enum ServerCommand {
    /// A one-time link to add a passkey (run as root, or as the server's user, on the server).
    AddPasskey {
        /// The server's admin socket, if it isn't in the usual place.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .with_writer(std::io::stderr)
        .init();
    let config_path = args.config.unwrap_or_else(agentflow::config::Config::default_path);
    use agentflow::connect;
    match args.command {
        Command::Connect { url } => connect::connect(&url),
        Command::Disconnect => connect::disconnect(&config_path),
        Command::Status(client) => connect::status(&config_path, &client),
        Command::Update(client) => connect::update(&config_path, &client),
        Command::Token { command, client } => match command {
            TokenCommand::Create { kind, project, name, file } => {
                if !matches!(kind.as_str(), "project" | "consultant") {
                    anyhow::bail!("create a `project <project>` or a `consultant` token");
                }
                connect::token_create(&config_path, &client, &kind, project.as_deref(), name.as_deref(), file.as_deref())
            }
            TokenCommand::List => connect::token_list(&config_path, &client),
            TokenCommand::Revoke { id } => connect::token_revoke(&config_path, &client, id),
            TokenCommand::Rotate { id, file } => connect::token_rotate(&config_path, &client, id, file.as_deref()),
        },
        Command::Project { command, client } => match command {
            ProjectCommand::Add { slug, name } => connect::project_add(&config_path, &client, &slug, &name),
            ProjectCommand::List => connect::project_list(&config_path, &client),
            ProjectCommand::Setup { folder, code, name } => connect::project_setup(&config_path, &client, folder, code, name),
        },
        Command::Backup(client) => connect::backup(&config_path, &client),
        Command::ImportPrs { project, range, client } => connect::import_prs(&config_path, &client, &project, &range),
        Command::Server { command: ServerCommand::AddPasskey { socket } } => connect::add_passkey(socket.as_deref()),
        Command::Builder { command, client } => {
            use agentflow::builder_cmd as b;
            match command {
                BuilderCommand::Add { name, user, current_user } => b::add(&config_path, &client, b::AddArgs { name, user, current_user }),
                BuilderCommand::List => b::list(&config_path, &client),
                BuilderCommand::Edit { name, rename, min_threads, min_memory, reserve_memory, keep_days } => {
                    b::edit(&config_path, &client, b::EditArgs { name, rename, min_threads, min_memory, reserve_memory, keep_days })
                }
                BuilderCommand::Remove { name } => b::remove(&config_path, &client, name),
            }
        }
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
        Command::Sync { client, all, files } => agentflow::sync::run(&config_path, &client, all, files),
    }
}
