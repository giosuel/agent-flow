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
    /// Projects (the New project button comes later).
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
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
    }
    Ok(())
}

/// Slugs end up in record ids and URLs: lowercase letters, digits and dashes.
pub fn check_slug(slug: &str) -> anyhow::Result<()> {
    let ok = !slug.is_empty()
        && slug.len() <= 40
        && slug.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !slug.starts_with('-');
    if !ok {
        bail!("{slug:?}: use lowercase letters, digits and dashes, e.g. \"cold-trace\"");
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
    let me = client.get("/api/whoami")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        for ok in ["vinculum", "cold-trace", "p2"] {
            assert!(check_slug(ok).is_ok(), "{ok}");
        }
        for bad in ["", "Cold", "a b", "-x", "a/b", "ä"] {
            assert!(check_slug(bad).is_err(), "{bad}");
        }
    }
}
