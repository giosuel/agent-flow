//! Command-line commands: `admin` works on the database directly (on the server), the rest talk to
//! the server over HTTP with a token.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::{Args, Subcommand};

use crate::activity::{self, Action, Entry, Via};
use crate::auth::{self, Actor, TokenKind};
use crate::config::Config;
use crate::db::Db;
use crate::util::{expand_home, fmt_utc};

#[derive(Subcommand)]
pub enum AdminCommand {
    /// Print a one-time link that signs a browser in as the owner (valid 15 minutes).
    LoginLink,
    /// API tokens for devices, project agents and the consultant.
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Projects (the web page's New project button also makes the folder and token).
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Back up the database now and export every project as Markdown (also done once a day).
    Backup,
    /// Print the root command that installs Claude Code managed settings keeping agents' file
    /// tools away from AgentFlow's tokens and data.
    ManagedSettings,
}

#[derive(Subcommand)]
pub enum TokenCommand {
    /// Create a token and print it once.
    Create {
        /// device, project or admin
        #[arg(long)]
        kind: TokenKind,
        /// What it's for, e.g. "HEADQUARTERS viber-run" or "Vinculum agent".
        #[arg(long)]
        name: String,
        /// The project, for a project token.
        #[arg(long)]
        project: Option<String>,
        /// Write the token to this file (mode 600) instead of printing it.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// List tokens (never their values).
    List,
    /// Revoke a token by its id.
    Revoke { id: i64 },
}

#[derive(Subcommand)]
pub enum ProjectCommand {
    /// Add a project.
    Add {
        /// Short id used in record ids and URLs, e.g. "vinculum".
        slug: String,
        /// Display name, e.g. "Vinculum".
        #[arg(long)]
        name: String,
    },
}

#[derive(Args)]
pub struct ClientArgs {
    /// The server, e.g. https://flow.giosuel.io (default: the config's public_url).
    #[arg(long, env = "AGENTFLOW_URL")]
    pub url: Option<String>,
    /// File holding your token (default: ~/.config/agentflow/token).
    #[arg(long, env = "AGENTFLOW_TOKEN_FILE")]
    pub token_file: Option<PathBuf>,
}

fn open_db(config_path: &Path) -> anyhow::Result<(Config, Db)> {
    let config = Config::load(config_path)?;
    let db = Db::open(&config.db_path())?;
    Ok((config, db))
}

fn log_cli(db: &Db, action: Action, summary: String) -> anyhow::Result<()> {
    let e = Entry::new(&Actor::System("admin-cli"), Via::Cli, action, summary);
    db.call_blocking(|c| Ok(activity::write_conn(c, &e)?))?;
    Ok(())
}

/// Commands that hand out access (a sign-in link, a token) want a person at a terminal. Agents
/// run as the same Unix user and could still fake one; this keeps them from doing it in passing.
/// Real separation is AgentFlow running as its own user.
fn require_terminal(what: &str) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        bail!(
            "{what} hands out access, so it only runs in an interactive terminal (yours), not from a script or an agent"
        );
    }
    Ok(())
}

pub fn admin(config_path: &Path, command: AdminCommand) -> anyhow::Result<()> {
    match &command {
        AdminCommand::LoginLink => require_terminal("login-link")?,
        AdminCommand::Token { command: TokenCommand::Create { .. } } => require_terminal("token create")?,
        _ => {}
    }
    let (config, db) = open_db(config_path)?;
    match command {
        AdminCommand::LoginLink => {
            let secret = db.call_blocking(|c| auth::create_login_link(c, "signin"))?;
            log_cli(&db, Action::Login, "one-time sign-in link created".into())?;
            println!("{}/login/link/{secret}", config.public_url.trim_end_matches('/'));
            eprintln!("Open it within {} minutes; it works once.", auth::LINK_MINUTES);
        }
        AdminCommand::Token { command: TokenCommand::Create { kind, name, project, out } } => {
            let (id, secret) = db.call_blocking(|c| auth::create_token(c, kind, &name, project.as_deref()))?;
            log_cli(&db, Action::Write, format!("{} token {id} created: {name}", kind.as_str()))?;
            match out {
                Some(path) => {
                    write_secret_file(&path, &secret)?;
                    eprintln!("Token {id} written to {}", path.display());
                }
                None => {
                    println!("{secret}");
                    eprintln!("Token {id}. It is shown only now; store it in a file only you can read.");
                }
            }
        }
        AdminCommand::Token { command: TokenCommand::List } => {
            let tokens = db.call_blocking(|c| auth::list_tokens(c))?;
            println!("id   kind     name                         project        last used            state");
            for t in tokens {
                println!(
                    "{:<4} {:<8} {:<28} {:<14} {:<20} {}",
                    t.id,
                    t.kind,
                    t.name,
                    t.project.unwrap_or_default(),
                    t.last_used_at.map(fmt_utc).unwrap_or_else(|| "never".into()),
                    if t.revoked_at.is_some() { "revoked" } else { "active" },
                );
            }
        }
        AdminCommand::Token { command: TokenCommand::Revoke { id } } => {
            if !db.call_blocking(|c| auth::revoke_token(c, id))? {
                bail!("no active token {id}");
            }
            log_cli(&db, Action::Write, format!("token {id} revoked"))?;
            eprintln!("Token {id} revoked.");
        }
        AdminCommand::Project { command: ProjectCommand::Add { slug, name } } => {
            db.call_blocking(|c| crate::projects::create(c, &slug, &name, "admin-cli"))?;
            log_cli(&db, Action::Write, format!("project {slug} added"))?;
            eprintln!("Project {slug} added.");
        }
        AdminCommand::Backup => {
            let data = config.data_dir();
            // "2026-11-11-0530": a manual one sorts after that day's nightly one.
            let now = crate::util::now_ms();
            let stamp = format!("{}-{}", crate::util::fmt_date(now), crate::util::fmt_hm(now).replace(':', ""));
            let (made, n) = db.call_blocking(move |c| {
                let made = crate::backup::backup(c, &data, &stamp)?;
                Ok((made, crate::backup::export(c, &data)?))
            })?;
            log_cli(&db, Action::Write, "backup and export by hand".into())?;
            if let Some(p) = made {
                eprintln!("Backed up to {}", p.display());
            }
            eprintln!("Exported {n} projects to {}", config.data_dir().join("export").display());
        }
        AdminCommand::ManagedSettings => print!("{}", crate::hardening::managed_settings_command(&config)),
    }
    Ok(())
}

fn write_secret_file(path: &Path, secret: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {} (it must not exist yet)", path.display()))?;
    writeln!(f, "{secret}")?;
    Ok(())
}

fn read_token(path: &Path) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading the token from {}", path.display()))?;
    let token = text.trim();
    if !token.starts_with("af_") {
        bail!("{} doesn't hold an AgentFlow token", path.display());
    }
    Ok(token.to_owned())
}

/// An HTTP client for the server, with the caller's token.
pub struct Client {
    pub base: String,
    token: String,
    http: reqwest::blocking::Client,
}

impl Client {
    pub fn new(config_path: &Path, args: &ClientArgs) -> anyhow::Result<Self> {
        let base = match &args.url {
            Some(u) => u.clone(),
            None => Config::load(config_path)?.public_url,
        };
        let token_file = args.token_file.clone().unwrap_or_else(|| expand_home("~/.config/agentflow/token"));
        let token = read_token(&token_file)?;
        let http = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(30)).build()?;
        Ok(Self { base: base.trim_end_matches('/').to_owned(), token, http })
    }

    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.http = reqwest::blocking::Client::builder().timeout(timeout).build().expect("http client");
        self
    }

    /// Uploads a file as the request body (a build archive) and returns the JSON answer.
    pub fn post_file(&self, path: &str, file: std::fs::File) -> anyhow::Result<serde_json::Value> {
        let len = file.metadata()?.len();
        let res = self
            .http
            .post(format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .header("Content-Type", "application/x-tar")
            .body(reqwest::blocking::Body::sized(file, len))
            .send()?;
        let status = res.status();
        let body: serde_json::Value = res.json().unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            bail!("{status}: {}", body.get("error").and_then(|e| e.as_str()).unwrap_or("upload failed"));
        }
        Ok(body)
    }

    /// Streams a response body into a file.
    pub fn download(&self, path: &str, to: &Path) -> anyhow::Result<()> {
        let mut res = self.http.get(format!("{}{path}", self.base)).bearer_auth(&self.token).send()?;
        if !res.status().is_success() {
            bail!("{}: download failed", res.status());
        }
        let mut f = std::fs::File::create(to)?;
        res.copy_to(&mut f)?;
        Ok(())
    }

    pub fn get(&self, path: &str) -> anyhow::Result<serde_json::Value> {
        let res = self.http.get(format!("{}{path}", self.base)).bearer_auth(&self.token).send()?;
        let status = res.status();
        let body: serde_json::Value = res.json().unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            bail!("{status}: {}", body.get("error").and_then(|e| e.as_str()).unwrap_or("request failed"));
        }
        Ok(body)
    }
}

pub fn wait(config_path: &Path, args: ClientArgs, record: Option<String>, minutes: u64) -> anyhow::Result<()> {
    let client = Client::new(config_path, &args)?.with_timeout(std::time::Duration::from_secs(30 * 60));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(minutes * 60);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now()).as_secs();
        if left == 0 {
            eprintln!("Nothing happened within {minutes} minutes.");
            std::process::exit(2);
        }
        let mut path = format!("/api/wait?timeout={}", left.min(25 * 60));
        if let Some(r) = &record {
            path.push_str(&format!("&record={}", url_encode(r)));
        }
        let res = match client.get(&path) {
            Ok(r) => r,
            Err(e) => {
                // The server restarting or a network blip shouldn't end the wait.
                eprintln!("waiting: {e:#}; retrying in 10 s");
                std::thread::sleep(std::time::Duration::from_secs(10));
                continue;
            }
        };
        let events = res.get("events").and_then(|e| e.as_array()).cloned().unwrap_or_default();
        if events.is_empty() {
            continue;
        }
        for e in events {
            let s = |k: &str| e.get(k).and_then(|v| v.as_str()).unwrap_or("").to_owned();
            println!("{} UTC  {}  (by {})", s("at"), s("what"), s("by"));
        }
        return Ok(());
    }
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub fn whoami(config_path: &Path, args: ClientArgs) -> anyhow::Result<()> {
    let client = Client::new(config_path, &args)?;
    let mut me = client.get("/api/whoami")?;
    // The same four characters the Settings page shows next to the token.
    me["token"] = format!("…{}", &client.token[client.token.len().saturating_sub(4)..]).into();
    println!("{}", serde_json::to_string_pretty(&me)?);
    Ok(())
}

/// Claude Code runs this as `headersHelper` when it connects, so the token never has to sit in
/// ~/.claude.json.
pub fn mcp_headers(token_file: &Path) -> anyhow::Result<()> {
    let token = read_token(token_file)?;
    println!("{}", serde_json::json!({ "Authorization": format!("Bearer {token}") }));
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Test builds: publish (agents) and run (the owner's machines)

#[derive(Args)]
pub struct PublishArgs {
    #[command(flatten)]
    pub client: ClientArgs,
    /// A binary, or a folder with the app and its files.
    pub path: PathBuf,
    /// The product, e.g. "imaginarium" (your project's build_product, or its slug).
    #[arg(long)]
    pub product: String,
    /// One line: what's in it.
    #[arg(long)]
    pub summary: String,
    /// The git checkout it was built from: its commit and branch name the build.
    #[arg(long)]
    pub checkout: Option<PathBuf>,
    /// For a folder: the executable inside it that starts the app.
    #[arg(long)]
    pub entry: Option<String>,
    /// Something for the owner to try, "Title: what to do". Repeat for each. With at least one,
    /// the owner is notified and gets a feedback form.
    #[arg(long = "feature")]
    pub features: Vec<String>,
    /// A tag this build is, e.g. v0.0.5-m4.
    #[arg(long)]
    pub tag: Option<String>,
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn query_pairs(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| format!("{k}={}", url_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn publish(config_path: &Path, a: PublishArgs) -> anyhow::Result<()> {
    let client = Client::new(config_path, &a.client)?.with_timeout(std::time::Duration::from_secs(30 * 60));
    if !a.path.exists() {
        bail!("{} doesn't exist", a.path.display());
    }
    let (commit, branch) = match &a.checkout {
        Some(dir) => (
            git_out(dir, &["rev-parse", "--short=7", "HEAD"]).context("the checkout isn't a git repository")?,
            git_out(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default(),
        ),
        None => (String::new(), String::new()),
    };
    // Pack next to the user's cache, not in /tmp: builds are big and /tmp is small here.
    let cache = expand_home("~/.cache/agentflow");
    std::fs::create_dir_all(&cache)?;
    let mut tmp = tempfile::NamedTempFile::new_in(&cache)?;
    crate::builds::pack(&a.path, tmp.as_file_mut())?;
    let file = tmp.reopen()?;
    let query = query_pairs(&[
        ("product", a.product.clone()),
        ("summary", a.summary.clone()),
        ("commit", commit),
        ("branch", branch),
        ("entry", a.entry.clone().unwrap_or_default()),
        ("tag", a.tag.clone().unwrap_or_default()),
        ("features", if a.features.is_empty() { String::new() } else { serde_json::to_string(&a.features)? }),
    ]);
    let res = client.post_file(&format!("/api/builds?{query}"), file)?;
    println!(
        "Published {} as #{} ({}). The owner runs it with: {}",
        a.product,
        res["number"],
        res["name"].as_str().unwrap_or(""),
        res["run"].as_str().unwrap_or("")
    );
    Ok(())
}

#[derive(Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub client: ClientArgs,
    /// The product; leave out to list them.
    pub product: Option<String>,
    /// A build's number or part of its name, or "list". The newest if left out.
    pub build: Option<String>,
    /// Arguments for the app (after --).
    #[arg(last = true)]
    pub args: Vec<String>,
}

pub fn run(config_path: &Path, a: RunArgs) -> anyhow::Result<()> {
    let client = Client::new(config_path, &a.client)?.with_timeout(std::time::Duration::from_secs(30 * 60));
    let Some(product) = a.product else {
        let list = client.get("/api/builds")?;
        for p in list["products"].as_array().cloned().unwrap_or_default() {
            println!(
                "{:<16} {:>3} builds   newest #{} {}",
                p["product"].as_str().unwrap_or(""),
                p["builds"],
                p["latest"]["number"],
                p["latest"]["summary"].as_str().unwrap_or("")
            );
        }
        return Ok(());
    };
    crate::builds::check_product(&product)?;
    let builds = client.get(&format!("/api/builds/{product}"))?["builds"].as_array().cloned().unwrap_or_default();
    if builds.is_empty() {
        bail!("{product} has no builds");
    }
    let cache_root = expand_home("~/.local/share/agentflow/builds").join(&product);
    if a.build.as_deref() == Some("list") {
        for b in &builds {
            let name = b["name"].as_str().unwrap_or("");
            let here = if cache_root.join(name).join(".verified").exists() { "✓" } else { " " };
            println!("{here} #{:<4} {}  {}", b["number"], name, b["summary"].as_str().unwrap_or(""));
        }
        return Ok(());
    }
    let pick = match a.build.as_deref() {
        None => builds.first(),
        Some(want) => builds
            .iter()
            .find(|b| want.trim_start_matches('#').parse::<i64>().ok() == b["number"].as_i64())
            .or_else(|| builds.iter().find(|b| b["name"].as_str().is_some_and(|n| n.contains(want)))),
    }
    .with_context(|| format!("no build of {product} matches {:?}", a.build.as_deref().unwrap_or("")))?;
    let number = pick["number"].as_i64().unwrap_or(0);
    let name = pick["name"].as_str().unwrap_or("").to_owned();
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        bail!("the server sent an odd build name");
    }
    let dir = cache_root.join(&name);
    // ".verified" holds the manifest the build was checked against; an old empty one doesn't count.
    let cached: Option<crate::builds::Manifest> =
        std::fs::read(dir.join(".verified")).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let manifest = match cached {
        Some(m) => m,
        None => {
            eprintln!("Fetching {product} #{number} ({name})…");
            let detail = client.get(&format!("/api/builds/{product}/{number}"))?;
            let manifest = crate::builds::Manifest {
                files: serde_json::from_value(detail["files"].clone()).context("the build's file list")?,
                total: 0,
            };
            for f in &manifest.files {
                crate::builds::safe_relative(Path::new(&f.path))?;
            }
            std::fs::create_dir_all(&cache_root)?;
            let tmp = tempfile::NamedTempFile::new_in(&cache_root)?;
            client.download(&format!("/api/builds/{product}/{number}/archive"), tmp.path())?;
            let staging = tempfile::tempdir_in(&cache_root)?;
            crate::builds::unpack_verified(std::fs::File::open(tmp.path())?, staging.path(), &manifest)?;
            std::fs::write(staging.path().join(".verified"), serde_json::to_vec(&manifest)?)?;
            let staged = staging.keep();
            if dir.exists() {
                std::fs::remove_dir_all(&dir)?;
            }
            if let Err(e) = std::fs::rename(&staged, &dir) {
                let _ = std::fs::remove_dir_all(&staged);
                // Another `agentflow run` of the same build may have finished first; that's fine.
                if !dir.join(".verified").exists() {
                    return Err(anyhow::anyhow!("keeping the build in {}: {e}", dir.display()));
                }
            }
            manifest
        }
    };
    let path = crate::builds::entry_path(&dir, &manifest, pick["entry"].as_str().unwrap_or(""))?;
    eprintln!("Starting {product} #{number}: {}", pick["summary"].as_str().unwrap_or(""));
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(&path).args(&a.args).current_dir(&dir).exec();
    Err(anyhow::anyhow!("starting {}: {err}", path.display()))
}
