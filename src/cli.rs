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
    /// Create "Lumen (demo)", a complete made-up project to try the pages with. Archive it on its
    /// page when you're done.
    Demo,
}

#[derive(Subcommand)]
pub enum TokenCommand {
    /// Create a token and print it once.
    Create {
        /// device, project or admin
        #[arg(long)]
        kind: TokenKind,
        /// What it's for, e.g. "HEADQUARTERS flow-run" or "Vinculum agent".
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
    config.apply();
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
        AdminCommand::ManagedSettings => print!("{}", crate::hardening::managed_settings_command(&config)?),
        AdminCommand::Demo => {
            let data = config.data_dir();
            let said = db.call_blocking(move |c| crate::demo::create(c, &data))?;
            log_cli(&db, Action::Write, format!("demo project {} created", crate::demo::SLUG))?;
            println!("{said}");
        }
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
    pub(crate) token: String,
    pub(crate) http: reqwest::blocking::Client,
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

    /// Sends JSON and returns the JSON answer.
    pub fn post_json(&self, path: &str, body: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let res = self.http.post(format!("{}{path}", self.base)).bearer_auth(&self.token).json(body).send()?;
        let status = res.status();
        let body: serde_json::Value = res.json().unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            bail!("{status}: {}", body.get("error").and_then(|e| e.as_str()).unwrap_or("request failed"));
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
    /// What it runs on: linux-x86_64 (the default), windows-x86_64, linux-aarch64, … Only Linux
    /// x86_64 builds are offered to flow-run here; others are a download on their page.
    #[arg(long)]
    pub platform: Option<String>,
    /// What it needs, one per flag, e.g. --needs "glibc 2.39 or newer" --needs "Vulkan (lavapipe works)".
    #[arg(long = "needs")]
    pub needs: Vec<String>,
}

#[derive(Args)]
pub struct ScreenshotArgs {
    #[command(flatten)]
    pub client: ClientArgs,
    /// The image: PNG, JPEG or WebP, up to 10 MB.
    pub file: PathBuf,
    /// What it shows, in a few words, e.g. "Settings sidebar, variant A".
    #[arg(long)]
    pub name: String,
    /// What kind of screenshot: test build, mockup, spike test, bug, … (a few words, your choice).
    #[arg(long)]
    pub category: String,
    /// More about it (Markdown), if the name doesn't say enough.
    #[arg(long, default_value = "")]
    pub description: String,
    /// The record it belongs to, e.g. T-12, B-3 or Q-4: it's linked and shows there.
    #[arg(long)]
    pub record: Option<String>,
    /// For a mockup: the design it shows, e.g. the claude.ai artifact link (with its page).
    #[arg(long)]
    pub design_url: Option<String>,
    /// For a mockup: proposed (default), approved or built.
    #[arg(long)]
    pub state: Option<String>,
}

#[derive(Args)]
pub struct RemoteArgs {
    #[command(flatten)]
    pub client: ClientArgs,
    /// Which build server (the first one if left out).
    #[arg(long)]
    pub server: Option<String>,
    /// A file or folder to bring back when it succeeds, e.g. target/release/app (repeat for more).
    /// target/… comes from the project's target folder there.
    #[arg(long = "fetch")]
    pub fetch: Vec<String>,
    /// Wait until the GPU there is idle (GPU tests).
    #[arg(long)]
    pub gpu: bool,
    /// Where to run it: a folder in your git checkout (default: here).
    #[arg(long, default_value = ".")]
    pub dir: PathBuf,
    /// The command, after `--`, e.g. `-- cargo build --release`.
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

/// Runs a build or test on a build server and prints its output as it comes; exits with its exit
/// code. Exit code 75 means the server is offline: build here instead.
#[derive(Args)]
pub struct NoteArgs {
    #[command(flatten)]
    pub client: ClientArgs,
    /// The file: any kind, up to 25 MB (a morning report, a plan, a benchmark, a log).
    pub file: PathBuf,
    /// What it is, in a few words, e.g. "Morning report, 12 Nov".
    #[arg(long, default_value = "")]
    pub name: String,
    /// A category of your choosing: morning report, overnight plan, estimate, known issue, …
    #[arg(long, default_value = "")]
    pub category: String,
    /// More about it (Markdown).
    #[arg(long, default_value = "")]
    pub description: String,
    /// A record it belongs to, e.g. T-12.
    #[arg(long)]
    pub record: Option<String>,
    /// When it should come back to you in hello: a date (2026-11-25) or a condition.
    #[arg(long)]
    pub resurface: Option<String>,
    /// A new version of this note's file, e.g. N-6 (the old one is kept).
    #[arg(long)]
    pub replaces: Option<String>,
}

/// Uploads a file as a note of the project.
pub fn note(config_path: &Path, a: NoteArgs) -> anyhow::Result<()> {
    let client = Client::new(config_path, &a.client)?.with_timeout(std::time::Duration::from_secs(300));
    let file = std::fs::File::open(&a.file).with_context(|| format!("can't open {}", a.file.display()))?;
    let filename = a.file.file_name().and_then(|n| n.to_str()).unwrap_or("note").to_owned();
    let mut q = vec![("name", a.name.clone()), ("category", a.category.clone()), ("filename", filename), ("description", a.description.clone())];
    for (k, v) in [("record", &a.record), ("resurface", &a.resurface), ("replaces", &a.replaces)] {
        if let Some(v) = v {
            q.push((k, v.clone()));
        }
    }
    let query = q.iter().map(|(k, v)| format!("{k}={}", url_encode(v))).collect::<Vec<_>>().join("&");
    let res = client.post_file(&format!("/api/notes?{query}"), file)?;
    println!("Uploaded as {}: {}", res["id"].as_str().unwrap_or(""), res["url"].as_str().unwrap_or(""));
    Ok(())
}

#[derive(Args)]
pub struct TimeArgs {
    #[command(flatten)]
    pub client: ClientArgs,
    /// What it's on: a task (T-12), a step (P9.3d) or a PR (#58). Left out: the one task in doing,
    /// or the one step under way.
    #[arg(long)]
    pub on: Option<String>,
    /// What to call it in the log; the command itself if left out.
    #[arg(long)]
    pub what: Option<String>,
    /// The command, after `--`, e.g. `-- cargo test --workspace`. It can start with a wrapper such
    /// as `systemd-run --user --scope -p MemoryMax=8G`.
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

/// Children's CPU (user + sys) from /proc/self/stat once they're waited for, in ms.
fn children_cpu_ms() -> Option<i64> {
    let text = std::fs::read_to_string("/proc/self/stat").ok()?;
    let f: Vec<&str> = text[text.rfind(')')? + 1..].split_whitespace().collect();
    // After ')': … cutime(13) cstime(14), in clock ticks (100 a second).
    let ticks: i64 = f.get(13)?.parse::<i64>().ok()? + f.get(14)?.parse::<i64>().ok()?;
    Some(ticks * 10)
}

/// The resident memory of a process and everything it started, in KB.
fn tree_rss_kb(root: u32) -> u64 {
    let mut parent: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let mut rss: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
    let Ok(list) = std::fs::read_dir("/proc") else { return 0 };
    for e in list.filter_map(Result::ok) {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        let Some(close) = text.rfind(')') else { continue };
        let f: Vec<&str> = text[close + 1..].split_whitespace().collect();
        if let (Some(pp), Some(r)) = (f.get(1).and_then(|x| x.parse().ok()), f.get(21).and_then(|x| x.parse::<u64>().ok())) {
            parent.insert(pid, pp);
            rss.insert(pid, r * 4);
        }
    }
    rss.iter()
        .filter(|(pid, _)| {
            let mut p = **pid;
            for _ in 0..64 {
                if p == root {
                    return true;
                }
                match parent.get(&p) {
                    Some(&pp) if pp > 1 => p = pp,
                    _ => return false,
                }
            }
            false
        })
        .map(|(_, kb)| kb)
        .sum()
}

/// Runs a command, measures wall time, CPU (user + sys of everything it started) and peak memory
/// (sampled), prints them and logs them in AgentFlow. Exits with the command's exit code.
pub fn time(config_path: &Path, a: TimeArgs) -> anyhow::Result<i32> {
    let client = Client::new(config_path, &a.client)?;
    let what = a.what.clone().unwrap_or_else(|| a.command.join(" ")).chars().take(200).collect::<String>();
    let start = std::time::Instant::now();
    let mut child = std::process::Command::new(&a.command[0])
        .args(&a.command[1..])
        .spawn()
        .with_context(|| format!("couldn't start {}", a.command[0]))?;
    let pid = child.id();
    let peak = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sampler = {
        let (peak, done) = (peak.clone(), done.clone());
        std::thread::spawn(move || {
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                let kb = tree_rss_kb(pid);
                peak.fetch_max(kb, std::sync::atomic::Ordering::Relaxed);
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        })
    };
    let status = child.wait()?;
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = sampler.join();
    let wall_ms = start.elapsed().as_millis() as i64;
    let cpu_ms = children_cpu_ms();
    let peak_mb = (peak.load(std::sync::atomic::Ordering::Relaxed) / 1024) as i64;
    let code = status.code().unwrap_or(1);
    eprintln!(
        "agentflow time: wall {}, CPU {}, peak {} (sampled), exit {code}",
        crate::times::fmt_wall(wall_ms),
        cpu_ms.map(|c| format!("{:.0} s", c as f64 / 1000.0)).unwrap_or_else(|| "?".into()),
        crate::times::fmt_mb(peak_mb),
    );
    let body = serde_json::json!({"what": what, "wall_ms": wall_ms, "cpu_ms": cpu_ms, "peak_mb": peak_mb,
        "exit_code": code, "on": a.on.clone().unwrap_or_default()});
    match client.post_json("/api/timings", &body) {
        Ok(r) => eprintln!("agentflow time: logged on {}", r["on"].as_str().unwrap_or("?")),
        Err(e) => eprintln!("agentflow time: not logged: {e:#}"),
    }
    Ok(code)
}

pub fn remote(config_path: &Path, a: RemoteArgs) -> anyhow::Result<i32> {
    let client = Client::new(config_path, &a.client)?.with_timeout(std::time::Duration::from_secs(60));
    let dir = std::fs::canonicalize(&a.dir).with_context(|| format!("{} doesn't exist", a.dir.display()))?;
    let body = serde_json::json!({
        "dir": dir, "command": a.command, "fetch": a.fetch, "gpu": a.gpu, "server": a.server,
    });
    let res =
        client.http.post(format!("{}/api/remote/jobs", client.base)).bearer_auth(&client.token).json(&body).send()?;
    let status = res.status();
    let v: serde_json::Value = res.json().unwrap_or(serde_json::Value::Null);
    if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
        eprintln!("{}", v["error"].as_str().unwrap_or("the build server is offline"));
        return Ok(75);
    }
    if !status.is_success() {
        bail!("{status}: {}", v["error"].as_str().unwrap_or("couldn't start the job"));
    }
    let id = v["id"].as_i64().context("no job id")?;
    eprintln!("Job #{id} on {}: {}", v["server"].as_str().unwrap_or(""), v["url"].as_str().unwrap_or(""));
    // Ctrl-C cancels the job there too.
    let (cancel_base, cancel_token) = (client.base.clone(), client.token.clone());
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        if rt.block_on(tokio::signal::ctrl_c()).is_ok() {
            drop(rt);
            let _ = reqwest::blocking::Client::new()
                .post(format!("{cancel_base}/api/remote/jobs/{id}/cancel"))
                .bearer_auth(&cancel_token)
                .send();
            eprintln!("\nCancelled job #{id}.");
            std::process::exit(130);
        }
    });
    let mut after = 0u64;
    let mut last_reason = String::new();
    loop {
        let o = client.get(&format!("/api/remote/jobs/{id}/output?after={after}"))?;
        let text = o["text"].as_str().unwrap_or("");
        if !text.is_empty() {
            print!("{text}");
            std::io::Write::flush(&mut std::io::stdout())?;
        }
        after = o["next"].as_u64().unwrap_or(after);
        let state = o["state"].as_str().unwrap_or("");
        let reason = o["reason"].as_str().unwrap_or("").to_owned();
        if matches!(state, "queued" | "paused") && reason != last_reason && !reason.is_empty() {
            eprintln!("· {state}: {reason}");
        }
        last_reason = reason;
        if o["finished"].as_bool().unwrap_or(false) && text.is_empty() {
            return Ok(match state {
                "done" => 0,
                "cancelled" => 130,
                _ => o["exit_code"].as_i64().filter(|c| *c != 0 && *c != 255).unwrap_or(1) as i32,
            });
        }
        if text.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(1000));
        }
    }
}

#[derive(Args)]
pub struct ContributeArgs {
    #[command(flatten)]
    pub client: ClientArgs,
    /// pr (a new PR: your commits on top of their branch), issue (a new issue), comment (on one
    /// already sent, --on C-4), or commits (new commits for a PR already sent, --on C-4).
    pub kind: String,
    /// Their repository, e.g. ophymx/formulary (pr, issue).
    #[arg(long, default_value = "")]
    pub repo: String,
    /// Their branch the PR goes into (pr).
    #[arg(long, default_value = "main")]
    pub base: String,
    /// The branch name on the owner's fork (pr), e.g. center-fences.
    #[arg(long, default_value = "")]
    pub branch: String,
    /// The PR's or issue's title.
    #[arg(long, default_value = "")]
    pub title: String,
    /// The text for them (Markdown), from a file; `-` reads it from stdin.
    #[arg(long)]
    pub body_file: Option<PathBuf>,
    /// For the owner only: why, in a sentence or two.
    #[arg(long, default_value = "")]
    pub why: String,
    /// Your project's task it's for, e.g. T-88.
    #[arg(long, default_value = "")]
    pub task: String,
    /// The owner's decision that agreed to it, e.g. D-31.
    #[arg(long, default_value = "")]
    pub agreed: String,
    /// The contribution a comment or commits are for, e.g. C-4.
    #[arg(long, default_value = "")]
    pub on: String,
    /// A new version of this draft (after the owner asked for changes), e.g. C-7.
    #[arg(long, default_value = "")]
    pub revise: String,
    /// The git checkout with the commits (pr, commits): everything on top of their branch up to
    /// HEAD is sent.
    #[arg(long, default_value = ".")]
    pub dir: PathBuf,
}

/// Sends a draft for someone else's repository to the owner, who sends it (or not) from its page.
pub fn contribute(config_path: &Path, a: ContributeArgs) -> anyhow::Result<()> {
    let client = Client::new(config_path, &a.client)?.with_timeout(std::time::Duration::from_secs(10 * 60));
    let body = match &a.body_file {
        Some(p) if p.as_os_str() == "-" => {
            let mut s = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
            s
        }
        Some(p) => std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?,
        None => String::new(),
    };
    let kind = a.kind.trim();
    // The commits: everything on top of their branch (or the PR's branch on the fork), as a bundle.
    let bundle = match kind {
        "pr" | "commits" => {
            let (url, branch) = if kind == "pr" {
                if a.repo.trim().is_empty() {
                    bail!("--repo names their repository, e.g. ophymx/formulary");
                }
                (format!("https://github.com/{}.git", a.repo.trim()), a.base.trim().to_owned())
            } else {
                let on = if a.on.trim().is_empty() { a.revise.trim() } else { a.on.trim() };
                let me = client.get("/api/whoami")?;
                let slug = me["project"].as_str().context("your token isn't a project's")?.to_owned();
                let c = client.get(&format!("/api/contributions/{slug}/{on}"))?;
                let fork =
                    c["fork"].as_str().filter(|f| !f.is_empty()).context("that PR wasn't sent through AgentFlow")?;
                (format!("https://github.com/{fork}.git"), c["branch"].as_str().unwrap_or("").to_owned())
            };
            let dir = &a.dir;
            let fetch = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["fetch", "-q", "--no-tags", &url, &branch])
                .status()
                .context("running git")?;
            if !fetch.success() {
                bail!("couldn't fetch {branch} of {url}");
            }
            let base = git_out(dir, &["rev-parse", "FETCH_HEAD"]).context("no FETCH_HEAD")?;
            let count = git_out(dir, &["rev-list", "--count", &format!("{base}..HEAD")]).unwrap_or_default();
            if count == "0" || count.is_empty() {
                bail!("HEAD has no commits on top of {branch}");
            }
            let tmp = tempfile::NamedTempFile::new_in(std::env::temp_dir())?;
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["bundle", "create", "-q"])
                .arg(tmp.path())
                .arg(format!("{base}..HEAD"))
                .status()?;
            if !ok.success() {
                bail!("git bundle failed");
            }
            use base64::Engine as _;
            println!("{count} commit(s) on top of {branch}.");
            base64::engine::general_purpose::STANDARD.encode(std::fs::read(tmp.path())?)
        }
        _ => String::new(),
    };
    let draft = serde_json::json!({
        "type": kind,
        "repo": a.repo.trim(),
        "base": a.base.trim(),
        "branch": a.branch.trim(),
        "title": a.title.trim(),
        "body": body,
        "why": a.why.trim(),
        "task": a.task.trim(),
        "agreed": a.agreed.trim(),
        "on": a.on.trim(),
        "revise": a.revise.trim(),
        "bundle": bundle,
    });
    let res = client.post_json("/api/contributions", &draft)?;
    println!(
        "Draft {} is waiting for the owner: {}
Nothing is posted until they press Send; their Send, \"Ask for changes\" or Drop reaches you like any other news.",
        res["id"].as_str().unwrap_or(""),
        res["url"].as_str().unwrap_or("")
    );
    Ok(())
}

/// Uploads a screenshot to the project's Screenshots.
pub fn screenshot(config_path: &Path, a: ScreenshotArgs) -> anyhow::Result<()> {
    let client = Client::new(config_path, &a.client)?.with_timeout(std::time::Duration::from_secs(120));
    let file = std::fs::File::open(&a.file).with_context(|| format!("opening {}", a.file.display()))?;
    let query = query_pairs(&[
        ("name", a.name.clone()),
        ("category", a.category.clone()),
        ("description", a.description.clone()),
        ("record", a.record.clone().unwrap_or_default()),
        ("design_url", a.design_url.clone().unwrap_or_default()),
        ("state", a.state.clone().unwrap_or_default()),
    ]);
    let res = client.post_file(&format!("/api/screenshots?{query}"), file)?;
    println!("Uploaded as {}: {}", res["id"].as_str().unwrap_or(""), res["url"].as_str().unwrap_or(""));
    Ok(())
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
        ("platform", a.platform.clone().unwrap_or_default()),
        ("needs", if a.needs.is_empty() { String::new() } else { serde_json::to_string(&a.needs)? }),
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
    let all = client.get(&format!("/api/builds/{product}"))?["builds"].as_array().cloned().unwrap_or_default();
    if all.is_empty() {
        bail!("{product} has no builds");
    }
    // Only builds for this machine (a Windows exe is a download on its page).
    let here = crate::builds::this_platform();
    let for_here = |b: &serde_json::Value| b["platform"].as_str().unwrap_or(crate::builds::DEFAULT_PLATFORM) == here;
    if let Some(want) = a.build.as_deref().filter(|w| *w != "list")
        && let Some(b) = all.iter().find(|b| want.trim_start_matches('#').parse::<i64>().ok() == b["number"].as_i64())
        && !for_here(b)
    {
        bail!(
            "#{} is for {}, not this machine ({here}); download it from its page: {}",
            b["number"],
            b["platform"].as_str().unwrap_or("?"),
            b["page"].as_str().unwrap_or("")
        );
    }
    let builds: Vec<serde_json::Value> = all.iter().filter(|b| for_here(b)).cloned().collect();
    if builds.is_empty() {
        bail!("{product} has no builds for {here}");
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
                // Another `flow-run` of the same build may have finished first; that's fine.
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
