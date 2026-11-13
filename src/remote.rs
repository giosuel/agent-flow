//! Build servers: agents run builds and tests on a stronger machine (the owner's workstation, over
//! WireGuard) with `flow remote -- <command>`. AgentFlow does the work, over ssh: it brings the
//! project's work folder there up to date, queues and starts the job when the machine has room, streams
//! its output to the agent, the job's page and the Log, and brings files back.
//!
//! The owner, 2026-11-11: "lets make sure cache is used effectively and as little as possible is
//! rebuilt or moved over the network all the time" and "lets make it so the cap is dynamic, based
//! on whether other processes are using it right now or not. we can also build multiple things if
//! there is enough processing power".
//!
//! - **Little over the network:** each project has one work folder there, a git repository AgentFlow
//!   pushes the agent's commit into (only new objects travel), then copies the uncommitted changes
//!   on top (rsync, by content). Git writes only files that changed, with a fresh time, so cargo
//!   rebuilds exactly what changed. The target folder stays next to it between jobs.
//! - **Dynamic limits:** a job runs in a systemd unit of its own, at low CPU and IO weight and
//!   niceness, so it gets what the owner isn't using. Another job starts while enough threads and
//!   memory are free; when memory runs short, the newest job is frozen until there's room again.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use crate::config::BuildServer;
use crate::util::now_ms;

// ---------------------------------------------------------------------------------------------
// ssh

/// The shared connection's socket: one ssh connection per server, reused by every probe and job.
fn control_path(data_dir: &Path) -> PathBuf {
    data_dir.join("remote").join("ssh-%C")
}

fn ssh_base(data_dir: &Path, server: &BuildServer, timeout_s: u64) -> Vec<String> {
    // Only the user's own config: AgentFlow's service runs with a private user namespace, where
    // root's /etc/ssh files look foreign and ssh refuses to read them.
    let user_config = crate::util::expand_home("~/.ssh/config");
    let config = if user_config.is_file() { user_config.display().to_string() } else { "/dev/null".into() };
    let mut args = vec![
        "-F".into(),
        config,
        // A server added on the Settings page is trusted on first contact (its fingerprint is
        // kept in known_hosts); a changed host key is refused.
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        format!("ConnectTimeout={timeout_s}"),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ControlMaster=auto".into(),
        "-o".into(),
        format!("ControlPath={}", control_path(data_dir).display()),
        "-o".into(),
        "ControlPersist=120".into(),
    ];
    if !server.key_file.is_empty() {
        args.extend(["-i".into(), server.key_file.clone(), "-o".into(), "IdentitiesOnly=yes".into()]);
    }
    args
}

/// The `ssh` command git and rsync use for this server.
fn ssh_command(data_dir: &Path, server: &BuildServer) -> String {
    format!("{} {}", server.ssh_program, ssh_base(data_dir, server, 10).join(" "))
}

/// Runs a shell script there (with `sh`, whatever the login shell is) and returns its output.
pub async fn run_script(
    data_dir: &Path,
    server: &BuildServer,
    script: &str,
    timeout: Duration,
) -> anyhow::Result<String> {
    if server.kind == "daemon" {
        let script = script.to_owned();
        let (code, stdout, stderr) = crate::builders::hub()
            .ask(&server.name, |id| crate::builders::Request::Sh { id, script, stream: false, timeout_s: timeout.as_secs() }, timeout + Duration::from_secs(5))
            .await?;
        if code != 0 {
            bail!("{}", stderr.trim().lines().last().unwrap_or("failed"));
        }
        return Ok(stdout);
    }
    tokio::fs::create_dir_all(data_dir.join("remote")).await.ok();
    let mut cmd = tokio::process::Command::new(&server.ssh_program);
    cmd.args(ssh_base(data_dir, server, 5)).arg(&server.ssh).arg("sh -s");
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = cmd.spawn().context("running ssh")?;
    let mut stdin = child.stdin.take().context("ssh stdin")?;
    stdin.write_all(script.as_bytes()).await?;
    drop(stdin);
    let out = tokio::time::timeout(timeout, child.wait_with_output()).await.context("ssh took too long")??;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("{}", err.trim().lines().last().unwrap_or("ssh failed"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Quotes a word for `sh`.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_./=:,+@%".contains(&c)) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

// ---------------------------------------------------------------------------------------------
// What the server is doing: probes

/// The server as last seen.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Probe {
    pub at: i64,
    pub online: bool,
    pub error: String,
    pub model: String,
    pub threads: f64,
    /// Threads busy: everything, and the builds' share (the build user's processes).
    pub busy: f64,
    pub builds: f64,
    pub mem_total_gb: f64,
    pub mem_free_gb: f64,
    pub mem_builds_gb: f64,
    pub gpu_name: String,
    pub gpu_util: f64,
    pub disk_free_gb: f64,
    pub disk_total_gb: f64,
}

impl Probe {
    /// Threads the owner (everything that isn't a build) is using.
    pub fn yours(&self) -> f64 {
        (self.busy - self.builds).max(0.0)
    }
    pub fn free_threads(&self) -> f64 {
        (self.threads - self.busy).max(0.0)
    }
    pub fn mem_yours_gb(&self) -> f64 {
        (self.mem_total_gb - self.mem_free_gb - self.mem_builds_gb).max(0.0)
    }
}

const PROBE: &str = r#"echo "stat $(head -1 /proc/stat)"
echo "nproc $(nproc)"
grep -E '^(MemTotal|MemAvailable):' /proc/meminfo | sed 's/^/mem /'
u=$(id -u)
echo "slice $(grep usage_usec /sys/fs/cgroup/user.slice/user-$u.slice/cpu.stat 2>/dev/null | cut -d' ' -f2) $(cat /sys/fs/cgroup/user.slice/user-$u.slice/memory.current 2>/dev/null)"
echo "model $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2)"
echo "gpu $(nvidia-smi --query-gpu=name,utilization.gpu --format=csv,noheader,nounits 2>/dev/null | head -1)"
echo "disk $(df -Pk "$HOME" | tail -1)"
"#;

/// The counters a probe needs from the one before to work out CPU use.
#[derive(Debug, Clone, Copy, Default)]
pub struct Counters {
    at: i64,
    total: u64,
    idle: u64,
    slice_usec: u64,
}

/// Reads the probe script's output; CPU use is the difference to `prev`.
pub fn parse_probe(text: &str, prev: Option<Counters>, at: i64) -> (Probe, Counters) {
    let mut p = Probe { at, online: true, ..Default::default() };
    let mut c = Counters { at, ..Default::default() };
    for line in text.lines() {
        let (key, rest) = line.split_once(' ').unwrap_or((line, ""));
        let nums: Vec<u64> = rest.split_whitespace().filter_map(|w| w.parse().ok()).collect();
        match key {
            "stat" => {
                // cpu user nice system idle iowait irq softirq steal …
                c.total = nums.iter().take(8).sum();
                c.idle = nums.get(3).copied().unwrap_or(0) + nums.get(4).copied().unwrap_or(0);
            }
            "nproc" => p.threads = nums.first().copied().unwrap_or(0) as f64,
            "mem" if rest.starts_with("MemTotal:") => {
                p.mem_total_gb = nums.first().copied().unwrap_or(0) as f64 / 1048576.0
            }
            "mem" if rest.starts_with("MemAvailable:") => {
                p.mem_free_gb = nums.first().copied().unwrap_or(0) as f64 / 1048576.0
            }
            "slice" => {
                c.slice_usec = nums.first().copied().unwrap_or(0);
                p.mem_builds_gb = nums.get(1).copied().unwrap_or(0) as f64 / 1073741824.0;
            }
            "model" => p.model = rest.trim().to_owned(),
            "gpu" => {
                if let Some((name, util)) = rest.rsplit_once(',') {
                    p.gpu_name = name.trim().to_owned();
                    p.gpu_util = util.trim().parse().unwrap_or(0.0);
                }
            }
            "disk" => {
                // filesystem 1K-blocks used available capacity mount
                let n: Vec<u64> = rest.split_whitespace().skip(1).filter_map(|w| w.parse().ok()).collect();
                p.disk_total_gb = n.first().copied().unwrap_or(0) as f64 / 1048576.0;
                p.disk_free_gb = n.get(2).copied().unwrap_or(0) as f64 / 1048576.0;
            }
            _ => {}
        }
    }
    if let Some(prev) = prev
        && c.total > prev.total
        && c.at > prev.at
    {
        let busy = 1.0 - (c.idle.saturating_sub(prev.idle)) as f64 / (c.total - prev.total) as f64;
        p.busy = (busy * p.threads).clamp(0.0, p.threads);
        let secs = (c.at - prev.at) as f64 / 1000.0;
        p.builds = ((c.slice_usec.saturating_sub(prev.slice_usec)) as f64 / 1e6 / secs).clamp(0.0, p.busy);
    }
    (p, c)
}

// ---------------------------------------------------------------------------------------------
// The live state, shared by the scheduler and the pages

#[derive(Default)]
pub struct ServerLive {
    pub probe: Probe,
    counters: Option<Counters>,
    /// The owner paused builds: nothing starts, running jobs are frozen.
    pub paused: bool,
    /// Each project's work folder there (and the shared cache), in GB, and when that was measured.
    pub sizes: Vec<(String, f64)>,
    sizes_at: i64,
    /// What's installed there, and when that was checked.
    pub tools: Tools,
    tools_at: i64,
    /// A setup step running or just finished: "installing sccache…", "sccache installed".
    pub setup: String,
    pub setup_running: bool,
}

impl ServerLive {
    /// Whether the tools have been looked at since AgentFlow started.
    pub fn tools_checked(&self) -> bool {
        self.tools_at > 0 && !self.setup_running
    }
}

/// The tools a build server has for Rust builds.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Tools {
    /// "cargo 1.99.1 (…)", empty when cargo or a default toolchain is missing.
    pub cargo: String,
    pub rustup: bool,
    /// "sccache 0.10.0", empty when it isn't installed.
    pub sccache: String,
    pub curl: bool,
}

/// Where the tools agents need are, for scripts and jobs there (non-login shells miss them).
const PATH_LINE: &str = "export PATH=\"$HOME/.local/bin:$HOME/.cargo/bin:$PATH\"\n";

pub async fn check_tools(data_dir: &Path, server: &BuildServer) -> anyhow::Result<Tools> {
    let script = format!(
        "{PATH_LINE}echo \"cargo $(cargo --version 2>/dev/null)\"\necho \"rustup $(command -v rustup)\"\necho \"sccache $(sccache --version 2>/dev/null)\"\necho \"curl $(command -v curl)\"\n"
    );
    let text = run_script(data_dir, server, &script, Duration::from_secs(30)).await?;
    let mut t = Tools::default();
    for line in text.lines() {
        let (k, v) = line.split_once(' ').unwrap_or((line, ""));
        let v = v.trim();
        match k {
            "cargo" => t.cargo = v.to_owned(),
            "rustup" => t.rustup = !v.is_empty(),
            "sccache" => t.sccache = v.to_owned(),
            "curl" => t.curl = !v.is_empty(),
            _ => {}
        }
    }
    Ok(t)
}

/// Checks the tools again and keeps the answer for the Settings page.
pub async fn refresh_tools(state: &crate::web::AppState, server: &BuildServer) -> anyhow::Result<Tools> {
    let t = check_tools(&state.config.data_dir(), server).await?;
    let mut live = state.remote.lock().unwrap_or_else(|e| e.into_inner());
    let s = live.servers.entry(server.name.clone()).or_default();
    s.tools = t.clone();
    s.tools_at = now_ms();
    Ok(t)
}

/// A setup step on a build server, in the background: its output goes to the Log
/// (`target:server-<name>`), its outcome to the Settings page.
pub fn spawn_setup(state: crate::web::AppState, server: BuildServer, what: &'static str) {
    {
        let mut live = state.remote.lock().unwrap_or_else(|e| e.into_inner());
        let s = live.servers.entry(server.name.clone()).or_default();
        if s.setup_running {
            return;
        }
        s.setup_running = true;
        s.setup = format!("{what}…");
    }
    tokio::spawn(async move {
        let script = match what {
            "installing sccache" => INSTALL_SCCACHE.to_owned(),
            _ => SETUP_RUST.to_owned(),
        };
        let data_dir = state.config.data_dir();
        let result = run_script(&data_dir, &server, &format!("{PATH_LINE}{script}"), Duration::from_secs(1800)).await;
        let target = format!("server-{}", server.name);
        let (outcome, sev) = match &result {
            Ok(out) => (format!("{what}: done. {}", out.lines().last().unwrap_or("").trim()), "info"),
            Err(e) => (format!("{what} failed: {e:#}"), "error"),
        };
        let lines: Vec<String> = match &result {
            Ok(out) => out.lines().map(str::to_owned).collect(),
            Err(e) => vec![format!("{e:#}")],
        };
        let e = crate::activity::Entry {
            actor_kind: "system".into(),
            actor: "build servers".into(),
            project: None,
            via: crate::activity::Via::Build,
            action: crate::activity::Action::System,
            summary: crate::activity::clip(&format!("{} on {}", outcome, server.name), 300),
            target: Some(target),
            status: None,
            duration_ms: None,
            ip: None,
            detail: Some(
                serde_json::json!({"lines": lines.iter().rev().take(40).rev().map(|l| crate::activity::redact(l)).collect::<Vec<_>>()}),
            ),
            severity: sev,
        };
        crate::activity::write(&state.db, e).await;
        let _ = refresh_tools(&state, &server).await;
        let mut live = state.remote.lock().unwrap_or_else(|e| e.into_inner());
        let s = live.servers.entry(server.name.clone()).or_default();
        s.setup_running = false;
        s.setup = outcome;
    });
}

/// sccache from its release on GitHub (a single file, into ~/.local/bin); with cargo when there's
/// no curl.
const INSTALL_SCCACHE: &str = r#"set -e
if command -v curl >/dev/null 2>&1; then
  v=$(curl -fsSL https://api.github.com/repos/mozilla/sccache/releases/latest | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)
  [ -n "$v" ] || { echo "couldn't find sccache's latest release" >&2; exit 1; }
  d=$(mktemp -d)
  curl -fsSL "https://github.com/mozilla/sccache/releases/download/$v/sccache-$v-$(uname -m)-unknown-linux-musl.tar.gz" | tar xz -C "$d"
  mkdir -p "$HOME/.local/bin"
  install -m 755 "$d"/sccache-*/sccache "$HOME/.local/bin/sccache"
  rm -rf "$d"
else
  cargo install --locked sccache
fi
sccache --version
"#;

/// A Rust toolchain for the build user: rustup's stable (rustup itself first, if it's missing).
const SETUP_RUST: &str = r#"set -e
if ! command -v rustup >/dev/null 2>&1; then
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path --default-toolchain none
fi
rustup default stable
cargo --version
"#;

/// Makes a key pair for a new build server in AgentFlow's data folder; returns the private key's
/// path and the public key.
pub async fn make_key(data_dir: &Path, name: &str) -> anyhow::Result<(String, String)> {
    let dir = data_dir.join("remote").join("keys");
    tokio::fs::create_dir_all(&dir).await?;
    let key = dir.join(name);
    if !key.exists() {
        let comment = format!("agentflow@{} for {name}", crate::machine::hostname());
        let out = tokio::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", &comment, "-f"])
            .arg(&key)
            .output()
            .await
            .context("running ssh-keygen")?;
        if !out.status.success() {
            bail!("ssh-keygen: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
    }
    let public = tokio::fs::read_to_string(key.with_extension("pub")).await?.trim().to_owned();
    Ok((key.to_string_lossy().to_string(), public))
}

/// The public key of a key AgentFlow made.
pub fn public_key(key_file: &str) -> Option<String> {
    if key_file.is_empty() {
        return None;
    }
    std::fs::read_to_string(format!("{key_file}.pub")).ok().map(|s| s.trim().to_owned())
}

#[derive(Default)]
pub struct Live {
    /// The build servers, from the database (the Settings page changes them).
    pub list: Vec<BuildServer>,
    pub servers: HashMap<String, ServerLive>,
    /// Running jobs' ssh processes, to stop them.
    running: HashMap<i64, tokio::sync::oneshot::Sender<()>>,
    /// When a page last asked: probes run every few seconds while someone looks or jobs run.
    pub watched_at: i64,
}

pub type Shared = Arc<Mutex<Live>>;

// ---------------------------------------------------------------------------------------------
// Jobs

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: i64,
    pub server: String,
    pub project_id: i64,
    pub project: String,
    pub actor: String,
    pub dir: String,
    pub subdir: String,
    pub command: Vec<String>,
    pub fetch: Vec<String>,
    pub gpu: bool,
    pub state: String,
    pub reason: String,
    pub exit_code: Option<i64>,
    pub commit_sha: String,
    pub dirty: i64,
    pub threads: Option<i64>,
    pub sent_bytes: Option<i64>,
    pub lines: i64,
    pub warnings: i64,
    pub errors: i64,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    /// A snapshot of the agent's working tree in the server's store (`flow remote`), for builders
    /// that connect themselves; empty: the server reads `dir` (the SSH path).
    pub manifest: String,
    pub bundle: String,
    /// Runs in the builder's home (`flow remote --setup`): installing tools, no working tree.
    pub setup: bool,
    /// What came back, kept on the server for `flow remote` to fetch.
    pub artifacts: Vec<String>,
}

impl Job {
    pub fn command_line(&self) -> String {
        self.command.iter().map(|w| sh_quote(w)).collect::<Vec<_>>().join(" ")
    }
    pub fn is_finished(&self) -> bool {
        matches!(self.state.as_str(), "done" | "failed" | "cancelled")
    }
    pub fn target(&self) -> String {
        format!("job-{}", self.id)
    }
}

const JOB_COLUMNS: &str =
    "j.id, j.server, j.project_id, p.slug, j.actor, j.dir, j.subdir, j.command, j.fetch, j.gpu, j.state,
     j.reason, j.exit_code, j.commit_sha, j.dirty, j.threads, j.sent_bytes, j.lines, j.warnings, j.errors, j.created_at,
     j.started_at, j.finished_at, j.manifest, j.bundle, j.setup, j.artifacts";

fn job_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    let list = |s: String| serde_json::from_str::<Vec<String>>(&s).unwrap_or_default();
    Ok(Job {
        id: r.get(0)?,
        server: r.get(1)?,
        project_id: r.get(2)?,
        project: r.get(3)?,
        actor: r.get(4)?,
        dir: r.get(5)?,
        subdir: r.get(6)?,
        command: list(r.get(7)?),
        fetch: list(r.get(8)?),
        gpu: r.get(9)?,
        state: r.get(10)?,
        reason: r.get(11)?,
        exit_code: r.get(12)?,
        commit_sha: r.get(13)?,
        dirty: r.get(14)?,
        threads: r.get(15)?,
        sent_bytes: r.get(16)?,
        lines: r.get(17)?,
        warnings: r.get(18)?,
        errors: r.get(19)?,
        created_at: r.get(20)?,
        started_at: r.get(21)?,
        finished_at: r.get(22)?,
        manifest: r.get(23)?,
        bundle: r.get(24)?,
        setup: r.get(25)?,
        artifacts: list(r.get(26)?),
    })
}

pub fn get(conn: &Connection, id: i64) -> anyhow::Result<Option<Job>> {
    Ok(conn
        .query_row(
            &format!("SELECT {JOB_COLUMNS} FROM remote_jobs j JOIN projects p ON p.id = j.project_id WHERE j.id = ?1"),
            [id],
            job_from_row,
        )
        .optional()?)
}

/// The newest jobs, of one server or all.
pub fn recent(conn: &Connection, server: Option<&str>, limit: i64) -> anyhow::Result<Vec<Job>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {JOB_COLUMNS} FROM remote_jobs j JOIN projects p ON p.id = j.project_id
         WHERE (?1 IS NULL OR j.server = ?1) ORDER BY j.id DESC LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![server, limit], job_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn in_state(conn: &Connection, server: &str, states: &[&str]) -> anyhow::Result<Vec<Job>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {JOB_COLUMNS} FROM remote_jobs j JOIN projects p ON p.id = j.project_id
         WHERE j.server = ?1 AND j.state IN (SELECT value FROM json_each(?2)) ORDER BY j.id"
    ))?;
    let rows = stmt.query_map(params![server, serde_json::to_string(states)?], job_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn set_state(conn: &Connection, id: i64, state: &str, reason: &str) -> anyhow::Result<()> {
    conn.execute("UPDATE remote_jobs SET state = ?2, reason = ?3 WHERE id = ?1", params![id, state, reason])?;
    Ok(())
}

/// A path to bring back: relative, inside the checkout (or its target folder), no `..`.
pub fn check_fetch(p: &str) -> anyhow::Result<()> {
    let ok = !p.is_empty()
        && !p.starts_with('/')
        && !p.starts_with('-')
        && p.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
        && p.bytes().all(|c| c.is_ascii_graphic());
    if !ok {
        bail!("{p:?}: bring back a path inside the checkout, e.g. target/release/app");
    }
    Ok(())
}

#[derive(Default)]
pub struct NewJob {
    pub server: String,
    pub project_id: i64,
    pub actor: String,
    pub dir: String,
    pub subdir: String,
    pub command: Vec<String>,
    pub fetch: Vec<String>,
    pub gpu: bool,
    pub manifest: String,
    pub bundle: String,
    pub setup: bool,
}

pub fn create(conn: &Connection, j: &NewJob) -> anyhow::Result<i64> {
    if j.command.is_empty() || j.command.iter().map(String::len).sum::<usize>() > 8000 {
        bail!("give the command to run, e.g. -- cargo build --release");
    }
    for f in &j.fetch {
        check_fetch(f)?;
    }
    conn.execute(
        "INSERT INTO remote_jobs (server, project_id, actor, dir, subdir, command, fetch, gpu, state, reason, created_at,
                                  manifest, bundle, setup)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'queued', 'waiting for its turn', ?9, ?10, ?11, ?12)",
        params![
            j.server,
            j.project_id,
            j.actor,
            j.dir,
            j.subdir,
            serde_json::to_string(&j.command)?,
            serde_json::to_string(&j.fetch)?,
            j.gpu,
            now_ms(),
            j.manifest,
            j.bundle,
            j.setup
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Jobs that were running when AgentFlow stopped: their ssh is gone, so they're over.
pub fn fail_orphans(conn: &Connection) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE remote_jobs SET state = 'failed', reason = 'AgentFlow restarted while it ran', finished_at = ?1
         WHERE state IN ('running', 'paused')",
        [now_ms()],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The scheduler

/// What decides whether a queued job may start now; None: it may.
pub fn wait_reason(server: &BuildServer, live: &ServerLive, job: &Job, running: &[Job]) -> Option<String> {
    let p = &live.probe;
    if !p.online {
        return Some(format!("{} is offline", server.name));
    }
    if live.paused {
        return Some("builds are paused".into());
    }
    if running.iter().any(|r| r.project_id == job.project_id) {
        return Some("another job of this project is running".into());
    }
    if p.free_threads() < server.min_threads {
        return Some(format!("waiting for {} free threads", server.min_threads));
    }
    if p.mem_free_gb < server.min_memory_gb + server.reserve_memory_gb.min(4.0) {
        return Some(format!("waiting for {} GB of free memory", server.min_memory_gb));
    }
    if job.gpu && p.gpu_util > 30.0 {
        return Some("waiting for the GPU to be idle".into());
    }
    None
}

// ---------------------------------------------------------------------------------------------
// The registry

/// The build servers now.
pub fn servers(state: &crate::web::AppState) -> Vec<BuildServer> {
    state.remote.lock().unwrap_or_else(|e| e.into_inner()).list.clone()
}

/// Reads the list again after a change.
pub async fn reload(state: &crate::web::AppState) {
    match state.db.call(|c| load_servers(c)).await {
        Ok(list) => {
            let ssh_program = state.config.build_servers.first().map(|b| b.ssh_program.clone());
            let list = list
                .into_iter()
                .map(|mut b| {
                    // Tests run a stand-in ssh; it's set in the config.
                    if let Some(p) = &ssh_program {
                        b.ssh_program = p.clone();
                    }
                    b
                })
                .collect();
            state.remote.lock().unwrap_or_else(|e| e.into_inner()).list = list;
        }
        Err(e) => tracing::warn!("build servers: {e:#}"),
    }
}

pub fn load_servers(conn: &Connection) -> anyhow::Result<Vec<BuildServer>> {
    let mut stmt = conn.prepare(
        "SELECT name, ssh, key_file, root, min_threads, min_memory_gb, reserve_memory_gb, keep_days, kind
         FROM build_servers ORDER BY created_at, name",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(BuildServer {
            name: r.get(0)?,
            ssh: r.get(1)?,
            key_file: r.get(2)?,
            root: r.get(3)?,
            min_threads: r.get(4)?,
            min_memory_gb: r.get(5)?,
            reserve_memory_gb: r.get(6)?,
            keep_days: r.get(7)?,
            kind: r.get(8)?,
            ..Default::default()
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Copies the servers in config.toml into the database (once; the Settings page manages them).
pub fn seed(conn: &Connection, from_config: &[BuildServer]) -> anyhow::Result<()> {
    for b in from_config {
        conn.execute(
            "INSERT OR IGNORE INTO build_servers (name, ssh, key_file, root, min_threads, min_memory_gb, reserve_memory_gb, keep_days, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![b.name, b.ssh, b.key_file, b.root, b.min_threads, b.min_memory_gb, b.reserve_memory_gb, b.keep_days, now_ms()],
        )?;
    }
    Ok(())
}

/// A name, an ssh target and a work folder AgentFlow accepts.
pub fn check_server(b: &BuildServer) -> anyhow::Result<()> {
    let simple =
        |s: &str| !s.is_empty() && s.len() <= 64 && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c));
    if !simple(&b.name) {
        bail!("the name is letters, digits, - _ . (e.g. headquarters)");
    }
    let target_ok = b.kind == "daemon"
        || !b.ssh.is_empty()
        && b.ssh.len() <= 200
        && !b.ssh.starts_with('-')
        && b.ssh.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.@:[]".contains(&c));
    if !target_ok {
        bail!("connect to user@host, or a Host from ~/.ssh/config");
    }
    if !b.root.split('/').all(|p| simple(p) && p != "." && p != "..") {
        bail!("the work folder is a folder in its home, like agentflow");
    }
    if !(b.min_threads >= 0.0 && b.min_memory_gb >= 0.0 && b.reserve_memory_gb >= 0.0 && b.keep_days >= 1) {
        bail!("the limits can't be negative, and folders are kept at least a day");
    }
    Ok(())
}

/// A builder that connects itself (`flow builder add`): no SSH target, its own token.
pub fn add_daemon(conn: &Connection, b: &BuildServer) -> anyhow::Result<()> {
    let probe = BuildServer { ssh: "daemon".into(), ..b.clone() };
    check_server(&probe)?;
    let n = conn.execute(
        "INSERT OR IGNORE INTO build_servers (name, ssh, key_file, root, min_threads, min_memory_gb, reserve_memory_gb, keep_days, created_at, kind)
         VALUES (?1, '', '', ?2, ?3, ?4, ?5, ?6, ?7, 'daemon')",
        params![b.name, b.root, b.min_threads, b.min_memory_gb, b.reserve_memory_gb, b.keep_days, now_ms()],
    )?;
    if n == 0 {
        bail!("a builder called {} exists already; flow builder edit or remove it", b.name);
    }
    Ok(())
}

pub fn add_server(conn: &Connection, b: &BuildServer) -> anyhow::Result<()> {
    check_server(b)?;
    let n = conn.execute(
        "INSERT OR IGNORE INTO build_servers (name, ssh, key_file, root, min_threads, min_memory_gb, reserve_memory_gb, keep_days, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![b.name, b.ssh, b.key_file, b.root, b.min_threads, b.min_memory_gb, b.reserve_memory_gb, b.keep_days, now_ms()],
    )?;
    if n == 0 {
        bail!("there's a build server called {} already", b.name);
    }
    Ok(())
}

pub fn set_limits(conn: &Connection, name: &str, b: &BuildServer) -> anyhow::Result<()> {
    check_server(b)?;
    conn.execute(
        "UPDATE build_servers SET min_threads = ?2, min_memory_gb = ?3, reserve_memory_gb = ?4, keep_days = ?5 WHERE name = ?1",
        params![name, b.min_threads, b.min_memory_gb, b.reserve_memory_gb, b.keep_days],
    )?;
    Ok(())
}

/// Removes a server; refused while jobs wait or run on it. Projects that picked it build locally.
pub fn remove_server(conn: &Connection, name: &str) -> anyhow::Result<()> {
    let busy: i64 = conn.query_row(
        "SELECT count(*) FROM remote_jobs WHERE server = ?1 AND state IN ('queued', 'running', 'paused')",
        [name],
        |r| r.get(0),
    )?;
    if busy > 0 {
        bail!("{name} has jobs waiting or running; cancel them first");
    }
    conn.execute("DELETE FROM project_build_server WHERE server = ?1", [name])?;
    conn.execute("DELETE FROM build_servers WHERE name = ?1", [name])?;
    // A builder that connected itself loses its token with it.
    conn.execute("UPDATE tokens SET revoked_at = ?2 WHERE kind = 'builder' AND name = ?1 AND revoked_at IS NULL", params![name, now_ms()])?;
    Ok(())
}

/// Renames a builder: its projects, its job history and its token (a builder connects by name)
/// move with it.
pub fn rename_server(conn: &mut Connection, old: &str, new: &str) -> anyhow::Result<()> {
    let probe = BuildServer { name: new.to_owned(), kind: "daemon".into(), ..Default::default() };
    check_server(&probe)?;
    let tx = conn.transaction()?;
    let taken: bool = tx.query_row("SELECT EXISTS (SELECT 1 FROM build_servers WHERE name = ?1)", [new], |r| r.get(0))?;
    if taken {
        bail!("there's a builder called {new} already");
    }
    if tx.execute("UPDATE build_servers SET name = ?2 WHERE name = ?1", params![old, new])? == 0 {
        bail!("no builder called {old}");
    }
    tx.execute("UPDATE project_build_server SET server = ?2 WHERE server = ?1", params![old, new])?;
    tx.execute("UPDATE remote_jobs SET server = ?2 WHERE server = ?1", params![old, new])?;
    tx.execute("UPDATE tokens SET name = ?2 WHERE kind = 'builder' AND name = ?1", params![old, new])?;
    tx.commit()?;
    Ok(())
}

/// The build server a project picked, if any.
pub fn project_server(conn: &Connection, project_id: i64) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row("SELECT server FROM project_build_server WHERE project_id = ?1", [project_id], |r| r.get(0))
        .optional()?)
}

/// Picks a project's build server ("" for none).
pub fn set_project_server(conn: &Connection, project_id: i64, server: &str) -> anyhow::Result<()> {
    if server.is_empty() {
        conn.execute("DELETE FROM project_build_server WHERE project_id = ?1", [project_id])?;
        return Ok(());
    }
    let known: bool =
        conn.query_row("SELECT EXISTS (SELECT 1 FROM build_servers WHERE name = ?1)", [server], |r| r.get(0))?;
    if !known {
        bail!("there's no build server called {server}");
    }
    conn.execute(
        "INSERT INTO project_build_server (project_id, server) VALUES (?1, ?2)
         ON CONFLICT (project_id) DO UPDATE SET server = excluded.server",
        params![project_id, server],
    )?;
    Ok(())
}

/// The work folder of a project there, relative to its home.
fn work_dir(server: &BuildServer, slug: &str) -> String {
    format!("{}/{}", server.root.trim_end_matches('/'), slug)
}

/// Runs the scheduler: probes the servers, starts what fits, pauses and resumes for memory.
pub fn spawn(state: crate::web::AppState) {
    tokio::spawn(async move {
        if let Err(e) = state.db.call(|c| fail_orphans(c)).await {
            tracing::warn!("build servers: {e:#}");
        }
        let from_config = state.config.build_servers.clone();
        if let Err(e) = state.db.call(move |c| seed(c, &from_config)).await {
            tracing::warn!("build servers: {e:#}");
        }
        reload(&state).await;
        let mut last_probe: HashMap<String, i64> = HashMap::new();
        let mut last_cleanup = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            for server in servers(&state) {
                if let Err(e) = round(&state, &server, &mut last_probe).await {
                    tracing::warn!("build server {}: {e:#}", server.name);
                }
            }
            if now_ms() - last_cleanup > 24 * 3600 * 1000 {
                last_cleanup = now_ms();
                for server in servers(&state) {
                    let st = state.clone();
                    tokio::spawn(async move {
                        if let Err(e) = cleanup(&st, &server).await {
                            tracing::warn!("build server {} cleanup: {e:#}", server.name);
                        }
                    });
                }
            }
        }
    });
}

async fn round(
    state: &crate::web::AppState,
    server: &BuildServer,
    last_probe: &mut HashMap<String, i64>,
) -> anyhow::Result<()> {
    let name = server.name.clone();
    let active = state.db.call(move |c| in_state(c, &name, &["queued", "running", "paused"])).await?;
    let now = now_ms();
    let watched = now - state.remote.lock().unwrap_or_else(|e| e.into_inner()).watched_at < 60_000;
    // Every 5 seconds while jobs wait or run or someone looks; once a minute otherwise.
    let every = if !active.is_empty() || watched { 5_000 } else { 60_000 };
    if now - last_probe.get(&server.name).copied().unwrap_or(0) < every {
        return Ok(());
    }
    last_probe.insert(server.name.clone(), now);
    let data_dir = state.config.data_dir();
    let probe = run_script(&data_dir, server, PROBE, Duration::from_secs(15)).await;
    let had_prev = {
        let mut live = state.remote.lock().unwrap_or_else(|e| e.into_inner());
        let s = live.servers.entry(server.name.clone()).or_default();
        let had_prev = s.counters.is_some();
        match &probe {
            Ok(text) => {
                if !s.probe.online {
                    tracing::info!("build server {} is online", server.name);
                }
                let (p, c) = parse_probe(text, s.counters, now_ms());
                s.probe = p;
                s.counters = Some(c);
            }
            Err(e) => {
                // Said once when it goes offline, not every minute while it stays off.
                if s.probe.online || s.probe.at == 0 {
                    tracing::info!("build server {} is offline: {e:#}", server.name);
                }
                s.probe = Probe { at: now_ms(), online: false, error: format!("{e:#}"), ..s.probe.clone() };
                s.counters = None;
            }
        }
        had_prev
    };
    // The folders' sizes, every 10 minutes while it's online.
    let measure = probe.is_ok() && {
        let mut live = state.remote.lock().unwrap_or_else(|e| e.into_inner());
        let s = live.servers.entry(server.name.clone()).or_default();
        let due = now - s.sizes_at > 600_000;
        if due {
            s.sizes_at = now;
        }
        due
    };
    if measure {
        let (st, srv) = (state.clone(), server.clone());
        tokio::spawn(async move {
            if let Ok(sizes) = sizes(&st.config.data_dir(), &srv).await {
                st.remote
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .servers
                    .entry(srv.name.clone())
                    .or_default()
                    .sizes = sizes;
            }
        });
    }
    // What's installed there, once an hour (and right after setup steps).
    let tools_due = probe.is_ok() && {
        let mut live = state.remote.lock().unwrap_or_else(|e| e.into_inner());
        let s = live.servers.entry(server.name.clone()).or_default();
        let due = now - s.tools_at > 3_600_000 && !s.setup_running;
        if due {
            s.tools_at = now;
        }
        due
    };
    if tools_due {
        let (st, srv) = (state.clone(), server.clone());
        tokio::spawn(async move {
            let _ = refresh_tools(&st, &srv).await;
        });
    }
    // The first probe can't know CPU use yet; decide on the next one.
    if probe.is_err() || active.is_empty() || !had_prev {
        return Ok(());
    }
    let (live_probe, paused) = {
        let live = state.remote.lock().unwrap_or_else(|e| e.into_inner());
        let s = &live.servers[&server.name];
        (s.probe.clone(), s.paused)
    };
    let running: Vec<Job> = active.iter().filter(|j| j.state == "running").cloned().collect();
    let frozen: Vec<Job> = active.iter().filter(|j| j.state == "paused").cloned().collect();
    // Memory short: freeze the newest running job. Room again (and not paused): thaw the oldest.
    if !running.is_empty() && (paused || live_probe.mem_free_gb < server.reserve_memory_gb) {
        let victims: Vec<&Job> = if paused { running.iter().collect() } else { running.iter().rev().take(1).collect() };
        for j in victims {
            let why = if paused { "builds are paused" } else { "memory is short: you're using it" };
            if freeze(&data_dir, server, j.id, true).await.is_ok() {
                let (id, why) = (j.id, why.to_owned());
                state.db.call(move |c| set_state(c, id, "paused", &why)).await?;
            }
        }
        return Ok(());
    }
    if let Some(j) = frozen.first()
        && !paused
        && live_probe.mem_free_gb > server.reserve_memory_gb + server.min_memory_gb / 2.0
    {
        if freeze(&data_dir, server, j.id, false).await.is_ok() {
            let id = j.id;
            state.db.call(move |c| set_state(c, id, "running", "")).await?;
        }
        return Ok(());
    }
    // One start per round, so the next probe sees what it takes.
    let busy: Vec<Job> = active.iter().filter(|j| j.state != "queued").cloned().collect();
    let live_guard = ServerLive { probe: live_probe.clone(), paused, ..Default::default() };
    for job in active.iter().filter(|j| j.state == "queued") {
        match wait_reason(server, &live_guard, job, &busy) {
            Some(reason) => {
                if reason != job.reason {
                    let id = job.id;
                    state.db.call(move |c| set_state(c, id, "queued", &reason)).await?;
                }
            }
            None => {
                let threads = live_probe.free_threads().floor().max(2.0) as i64;
                let id = job.id;
                state
                    .db
                    .call(move |c| {
                        c.execute(
                            "UPDATE remote_jobs SET state = 'running', reason = 'copying the changes', started_at = ?2, threads = ?3 WHERE id = ?1",
                            params![id, now_ms(), threads],
                        )?;
                        Ok(())
                    })
                    .await?;
                let (st, srv, job, probe) = (state.clone(), server.clone(), job.clone(), live_probe.clone());
                tokio::spawn(async move { run_job(st, srv, job, threads, probe).await });
                return Ok(());
            }
        }
    }
    Ok(())
}

async fn freeze(data_dir: &Path, server: &BuildServer, id: i64, on: bool) -> anyhow::Result<()> {
    let verb = if on { "freeze" } else { "thaw" };
    run_script(data_dir, server, &format!("systemctl --user {verb} af-job-{id}.service\n"), Duration::from_secs(20))
        .await?;
    Ok(())
}

/// Stops a job: the unit there, and the ssh streaming it.
pub async fn cancel(state: &crate::web::AppState, id: i64, by: &str) -> anyhow::Result<()> {
    let job = state.db.call(move |c| get(c, id)).await?.context("no such job")?;
    if job.is_finished() {
        return Ok(());
    }
    let server = servers(state).into_iter().find(|s| s.name == job.server).context("unknown server")?;
    let why = format!("cancelled by {by}");
    let w = why.clone();
    state
        .db
        .call(move |c| {
            c.execute(
                "UPDATE remote_jobs SET state = 'cancelled', reason = ?2, finished_at = ?3 WHERE id = ?1",
                params![id, w, now_ms()],
            )?;
            Ok(())
        })
        .await?;
    if job.state != "queued" {
        let _ = run_script(
            &state.config.data_dir(),
            &server,
            &format!(
                "systemctl --user thaw af-job-{id}.service 2>/dev/null; systemctl --user stop af-job-{id}.service\n"
            ),
            Duration::from_secs(30),
        )
        .await;
        if let Some(tx) = state.remote.lock().unwrap_or_else(|e| e.into_inner()).running.remove(&id) {
            let _ = tx.send(());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Running one job

/// Where a job's output is kept.
pub fn log_path(data_dir: &Path, id: i64) -> PathBuf {
    data_dir.join("remote").join("jobs").join(format!("{id}.log"))
}

/// A line's severity in the Log: compiler errors and warnings stand out.
pub fn line_severity(line: &str) -> &'static str {
    let l = line.trim_start();
    if l.starts_with("error")
        || l.starts_with("FAILED")
        || l.contains("panicked at")
        || l.starts_with("test result: FAILED")
    {
        "error"
    } else if l.starts_with("warning") {
        "warn"
    } else {
        "info"
    }
}

/// Output lines as Log entries, a batch at a time.
struct Out {
    file: tokio::fs::File,
    batch: Vec<(String, &'static str)>,
    lines: i64,
    warnings: i64,
    errors: i64,
    /// The lines worth seeing in the Log's entry, numbered: the first few, warnings and errors, the
    /// last few.
    highlights: Vec<(i64, String)>,
    first: Vec<(i64, String)>,
    last: std::collections::VecDeque<(i64, String)>,
}

impl Out {
    async fn line(&mut self, text: &str) {
        let _ = self.file.write_all(format!("{text}\n").as_bytes()).await;
        let sev = line_severity(text);
        self.lines += 1;
        match sev {
            "error" => self.errors += 1,
            "warn" => self.warnings += 1,
            _ => {}
        }
        let n = self.lines;
        if sev != "info" && self.highlights.len() < 40 {
            self.highlights.push((n, text.to_owned()));
        }
        if self.first.len() < 3 {
            self.first.push((n, text.to_owned()));
        }
        self.last.push_back((n, text.to_owned()));
        if self.last.len() > 4 {
            self.last.pop_front();
        }
        self.batch.push((text.to_owned(), sev));
    }
}

/// A note of AgentFlow's own in the job's output (dimmed on the page): "· synced …".
fn note(text: &str) -> String {
    format!("· {} {text}", crate::util::fmt_utc(now_ms()).get(11..19).unwrap_or(""))
}

async fn flush(state: &crate::web::AppState, job: &Job, out: &mut Out) {
    if out.batch.is_empty() {
        return;
    }
    let lines = std::mem::take(&mut out.batch);
    let (project, target, actor) = (job.project.clone(), job.target(), job.project.clone());
    let counts = (out.lines, out.warnings, out.errors, job.id);
    let r = state
        .db
        .call(move |c| {
            let tx = c.unchecked_transaction()?;
            for (text, sev) in &lines {
                let e = crate::activity::Entry {
                    actor_kind: "project".into(),
                    actor: actor.clone(),
                    project: Some(project.clone()),
                    via: crate::activity::Via::Build,
                    action: crate::activity::Action::Output,
                    summary: crate::activity::clip(&crate::activity::redact(text), 300),
                    target: Some(target.clone()),
                    status: None,
                    duration_ms: None,
                    ip: None,
                    detail: None,
                    severity: sev,
                };
                crate::activity::write_conn(&tx, &e)?;
            }
            tx.execute(
                "UPDATE remote_jobs SET lines = ?1, warnings = ?2, errors = ?3 WHERE id = ?4",
                params![counts.0, counts.1, counts.2, counts.3],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await;
    if let Err(e) = r {
        tracing::warn!("job {} output: {e:#}", job.id);
    }
}

/// Local commands (git, rsync) in the agent's checkout, with the server's ssh.
async fn local(dir: &str, program: &str, args: &[&str], ssh: &str, input: Option<&[u8]>) -> anyhow::Result<String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args).current_dir(dir).env("GIT_SSH_COMMAND", ssh).env("RSYNC_RSH", ssh).env("GIT_TERMINAL_PROMPT", "0");
    cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().with_context(|| format!("running {program}"))?;
    if let Some(data) = input {
        let mut stdin = child.stdin.take().context("stdin")?;
        stdin.write_all(data).await?;
    }
    let out =
        tokio::time::timeout(Duration::from_secs(1800), child.wait_with_output()).await.context("took too long")??;
    if !out.status.success() {
        bail!("{program}: {}", String::from_utf8_lossy(&out.stderr).trim().lines().last().unwrap_or("failed"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What `sync` did.
struct Synced {
    sha: String,
    changed: usize,
    sent: i64,
}

/// Brings the work folder there to the agent's state, writing only what differs, so cargo
/// rebuilds only what changed and an unchanged checkout costs nothing:
/// - the commit and tags go through git (only new objects travel); git there only moves its index
///   to that commit, never the files, so `git describe` and `--dirty` see what the agent sees;
/// - every file git would see (tracked and new, not ignored) goes through rsync, compared by
///   content: only files that differ are sent and rewritten, the rest keep their times;
/// - files gone here are deleted there.
async fn sync(data_dir: &Path, server: &BuildServer, job: &Job) -> anyhow::Result<Synced> {
    let ssh = ssh_command(data_dir, server);
    let work = format!("{}/work", work_dir(server, &job.project));
    let dir = job.dir.as_str();
    let sha = local(dir, "git", &["rev-parse", "HEAD"], &ssh, None).await?.trim().to_owned();
    run_script(
        data_dir,
        server,
        &format!(
            "set -e\nmkdir -p {w}\ncd {w}\n[ -d .git ] || git init -q .\ngit config receive.denyCurrentBranch ignore\ntouch ../.used\n",
            w = sh_quote(&work)
        ),
        Duration::from_secs(30),
    )
    .await
    .context("preparing the work folder")?;
    let url = format!("{}:{work}", server.ssh);
    local(dir, "git", &["push", "-q", "--force", "--no-verify", &url, "HEAD:refs/heads/agentflow"], &ssh, None)
        .await
        .context("sending the commit")?;
    // Tags too: versions come from them.
    let _ =
        local(dir, "git", &["push", "-q", "--force", "--no-verify", &url, "refs/tags/*:refs/tags/*"], &ssh, None).await;
    // Every file git sees here, that exists (a deleted file is still listed until it's staged).
    let listed =
        local(dir, "git", &["ls-files", "-z", "--cached", "--others", "--exclude-standard"], &ssh, None).await?;
    let mut files: Vec<&str> =
        listed.split('\0').filter(|f| !f.is_empty() && Path::new(dir).join(f).is_file()).collect();
    files.sort_unstable();
    files.dedup();
    let changed = local(dir, "git", &["ls-files", "-z", "-m", "-o", "--exclude-standard"], &ssh, None).await?;
    let dirty = changed.split('\0').filter(|f| !f.is_empty()).count();
    let list = files.join("\0");
    let stats = local(
        dir,
        "rsync",
        &["-rlpc", "--from0", "--files-from=-", "--stats", ".", &format!("{url}/")],
        &ssh,
        Some(list.as_bytes()),
    )
    .await
    .context("copying the files")?;
    let sent = stats
        .lines()
        .find(|l| l.starts_with("Total bytes sent:"))
        .and_then(|l| l.split(':').nth(1))
        .map(|n| n.trim().replace(',', "").parse().unwrap_or(0))
        .unwrap_or(0);
    // The list, so files gone here go there too; then git's index to the commit.
    let mut tmp = tempfile::NamedTempFile::new()?;
    std::io::Write::write_all(&mut tmp, list.as_bytes())?;
    let tmp_path = tmp.path().to_string_lossy().to_string();
    local(dir, "rsync", &["-c", &tmp_path, &format!("{url}/.git/af-files")], &ssh, None)
        .await
        .context("sending the file list")?;
    run_script(
        data_dir,
        server,
        &format!(
            "set -e\ncd {w}\ngit reset -q --mixed {sha}\ngit ls-files -z --cached --others --exclude-standard | sort -z > .git/af-there\nsort -z .git/af-files > .git/af-here\ncomm -z -23 .git/af-there .git/af-here | xargs -0 -r rm -f --\n",
            w = sh_quote(&work)
        ),
        Duration::from_secs(120),
    )
    .await
    .context("matching the commit")?;
    Ok(Synced { sha, changed: dirty, sent })
}

async fn run_job(state: crate::web::AppState, server: BuildServer, job: Job, threads: i64, probe: Probe) {
    let data_dir = state.config.data_dir();
    let path = log_path(&data_dir, job.id);
    if let Some(p) = path.parent() {
        let _ = tokio::fs::create_dir_all(p).await;
    }
    let file = match tokio::fs::File::create(&path).await {
        Ok(f) => f,
        Err(e) => {
            finish(&state, &job, "failed", &format!("can't write the output: {e}"), None, None).await;
            return;
        }
    };
    let mut out = Out {
        file,
        batch: Vec::new(),
        lines: 0,
        warnings: 0,
        errors: 0,
        highlights: Vec::new(),
        first: Vec::new(),
        last: Default::default(),
    };
    let started = crate::activity::Entry {
        actor_kind: "project".into(),
        actor: job.project.clone(),
        project: Some(job.project.clone()),
        via: crate::activity::Via::Build,
        action: crate::activity::Action::Run,
        summary: format!("job #{} on {} started: {}", job.id, server.name, job.command_line()),
        target: Some(job.target()),
        status: None,
        duration_ms: None,
        ip: None,
        detail: None,
        severity: "info",
    };
    crate::activity::write(&state.db, started).await;

    let daemon = server.kind == "daemon";
    if !job.setup {
        out.line(&note(&format!("copying {} to {}", if job.dir.is_empty() { "the snapshot" } else { &job.dir }, server.name))).await;
    }
    let synced = if job.setup {
        Ok(Synced { sha: String::new(), changed: 0, sent: 0 })
    } else if daemon {
        daemon_sync(&server, &job).await
    } else {
        sync(&data_dir, &server, &job).await
    };
    let synced = match synced {
        Ok(s) => s,
        Err(e) => {
            out.line(&note(&format!("couldn't copy the changes: {e:#}"))).await;
            flush(&state, &job, &mut out).await;
            finish(&state, &job, "failed", &format!("{e:#}"), None, Some(&out)).await;
            return;
        }
    };
    if !job.setup { out.line(&note(&format!(
        "at commit {} plus {} changed file{} ({} bytes sent)",
        synced.sha.get(..8).unwrap_or(""),
        synced.changed,
        if synced.changed == 1 { "" } else { "s" },
        synced.sent
    )))
    .await; }
    let (id, sha, changed, sent) = (job.id, synced.sha.clone(), synced.changed as i64, synced.sent);
    let _ = state
        .db
        .call(move |c| {
            c.execute(
                "UPDATE remote_jobs SET commit_sha = ?2, dirty = ?3, sent_bytes = ?4, reason = '' WHERE id = ?1",
                params![id, sha, changed, sent],
            )?;
            Ok(())
        })
        .await;

    // The command, in a unit of its own: low priority, a memory limit that leaves the owner room.
    let base = work_dir(&server, &job.project);
    let workdir = if job.setup {
        // `flow remote --setup`: tools go into the builder's home (the owner, 2026-11-13).
        ".".to_owned()
    } else if job.subdir.is_empty() {
        format!("{base}/work")
    } else {
        format!("{base}/work/{}", job.subdir)
    };
    let mem_high = (probe.mem_free_gb - server.reserve_memory_gb).max(8.0).floor();
    let mem_max = (probe.mem_total_gb - server.reserve_memory_gb).max(mem_high + 2.0).floor();
    let command = job.command_line();
    let script = format!(
        r#"export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
cd "$HOME"/{workdir} || exit 97
W=$(command -v sccache 2>/dev/null || true)
exec systemd-run --user --quiet --wait --pipe --collect --unit=af-job-{id} --working-directory="$PWD" \
  -p CPUWeight=10 -p IOWeight=10 -p Nice=10 -p MemoryHigh={mem_high}G -p MemoryMax={mem_max}G \
  -E PATH="$PATH" -E CARGO_TARGET_DIR="$HOME"/{base}/target -E CARGO_BUILD_JOBS={threads} -E CARGO_TERM_COLOR=never -E NO_COLOR=1 \
  ${{W:+-E RUSTC_WRAPPER="$W" -E SCCACHE_DIR="$HOME"/{root}/.sccache -E SCCACHE_CACHE_SIZE=60G}} \
  -- sh -c {cmd}
"#,
        workdir = sh_quote(&workdir),
        id = job.id,
        base = sh_quote(&base),
        root = sh_quote(&server.root),
        cmd = sh_quote(&format!("exec 2>&1; {command}")),
    );
    out.line(&note(&format!(
        "{command}   ({threads} threads to start with; low priority; memory up to {mem_high} GB)"
    )))
    .await;
    flush(&state, &job, &mut out).await;

    let code = if daemon {
        stream_daemon(&state, &server, &job, script, &mut out).await
    } else {
        match stream_ssh(&state, &data_dir, &server, &job, script, &mut out).await {
            Ok(c) => c,
            Err(e) => {
                finish(&state, &job, "failed", &format!("running ssh: {e}"), None, Some(&out)).await;
                return;
            }
        }
    };
    state.remote.lock().unwrap_or_else(|e| e.into_inner()).running.remove(&job.id);
    let id = job.id;
    if state.db.call(move |c| get(c, id)).await.ok().flatten().is_some_and(|j| j.state == "cancelled") {
        out.line(&note("cancelled")).await;
        flush(&state, &job, &mut out).await;
        log_finish(&state, &job, "cancelled", None, &out).await;
        return;
    }
    after_run(&state, &data_dir, &server, &job, code, &mut out).await;
}

/// Runs the job's script over ssh, its lines into the output. Returns the exit code (None: the
/// connection was lost or the job was stopped).
async fn stream_ssh(
    state: &crate::web::AppState,
    data_dir: &Path,
    server: &BuildServer,
    job: &Job,
    script: String,
    out: &mut Out,
) -> anyhow::Result<Option<i64>> {
    let mut cmd = tokio::process::Command::new(&server.ssh_program);
    cmd.args(ssh_base(data_dir, server, 10)).arg(&server.ssh).arg("sh -s");
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(script.as_bytes()).await;
    }
    let (tx, mut stop) = tokio::sync::oneshot::channel();
    state.remote.lock().unwrap_or_else(|e| e.into_inner()).running.insert(job.id, tx);
    let mut stdout = tokio::io::BufReader::new(child.stdout.take().expect("stdout")).lines();
    let mut stderr = tokio::io::BufReader::new(child.stderr.take().expect("stderr")).lines();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut stderr_open = true;
    let status = loop {
        tokio::select! {
            line = stdout.next_line() => match line {
                Ok(Some(l)) => out.line(&l).await,
                _ => break child.wait().await.ok(),
            },
            line = stderr.next_line(), if stderr_open => match line {
                Ok(Some(l)) => out.line(&l).await,
                _ => stderr_open = false,
            },
            _ = tick.tick() => flush(state, job, out).await,
            _ = &mut stop => {
                let _ = child.kill().await;
                break None;
            }
        }
    };
    Ok(status.and_then(|s| s.code()).map(i64::from))
}

/// Runs the job's script on a builder that connected itself. A dropped connection doesn't end
/// it: the builder sends the rest when it's back.
async fn stream_daemon(state: &crate::web::AppState, server: &BuildServer, job: &Job, script: String, out: &mut Out) -> Option<i64> {
    let (rid, mut events) = match crate::builders::hub().stream(&server.name, script) {
        Ok(x) => x,
        Err(e) => {
            out.line(&note(&format!("{e:#}"))).await;
            return None;
        }
    };
    let (tx, mut stop) = tokio::sync::oneshot::channel();
    state.remote.lock().unwrap_or_else(|e| e.into_inner()).running.insert(job.id, tx);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let code = loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Some(crate::builders::Event::Line(l)) => out.line(&l).await,
                Some(crate::builders::Event::Exit(c)) => break Some(i64::from(c)),
                Some(crate::builders::Event::Error(e)) => {
                    out.line(&note(&e)).await;
                    break None;
                }
                None => break None,
            },
            _ = tick.tick() => flush(state, job, out).await,
            _ = &mut stop => {
                crate::builders::hub().kill(&server.name, rid);
                break None;
            }
        }
    };
    crate::builders::hub().forget(rid);
    code
}

/// What follows a run: bring back what was asked for, and finish.
async fn after_run(state: &crate::web::AppState, data_dir: &Path, server: &BuildServer, job: &Job, code: Option<i64>, out: &mut Out) {
    let (state, server, job) = (state.clone(), server.clone(), job.clone());
    match code {
        Some(0) => {
            // Bring back what was asked for.
            let mut fetched = Vec::new();
            for f in &job.fetch {
                let got = if server.kind == "daemon" { daemon_fetch(&state, &server, &job, f).await } else { fetch(data_dir, &server, &job, f).await };
                match got {
                    Ok(()) => fetched.push(f.clone()),
                    Err(e) => out.line(&note(&format!("couldn't bring back {f}: {e:#}"))).await,
                }
            }
            if !fetched.is_empty() {
                out.line(&note(&format!("brought back {}", fetched.join(", ")))).await;
            }
            flush(&state, &job, out).await;
            let missing = fetched.len() < job.fetch.len();
            finish(
                &state,
                &job,
                if missing { "failed" } else { "done" },
                if missing { "not everything came back" } else { "" },
                code,
                Some(&*out),
            )
            .await;
        }
        Some(255) | None => {
            out.line(&note(&format!("lost the connection to {}", server.name))).await;
            flush(&state, &job, out).await;
            finish(&state, &job, "failed", &format!("lost the connection to {}", server.name), code, Some(&*out)).await;
        }
        Some(c) => {
            out.line(&note(&format!("exit code {c}"))).await;
            flush(&state, &job, out).await;
            finish(&state, &job, "failed", &format!("exit code {c}"), code, Some(&*out)).await;
        }
    }
}

/// Brings a builder's work folder to the job's snapshot (see `crate::builders`).
async fn daemon_sync(server: &BuildServer, job: &Job) -> anyhow::Result<Synced> {
    let work = format!("{}/work", work_dir(server, &job.project));
    let (manifest, bundle) = (job.manifest.clone(), job.bundle.clone());
    let (code, stdout, stderr) = crate::builders::hub()
        .ask(
            &server.name,
            |id| crate::builders::Request::Sync { id, work, manifest, bundle, commit: String::new() },
            Duration::from_secs(1800),
        )
        .await?;
    if code != 0 {
        bail!("{}", stderr.trim().lines().last().unwrap_or("the sync failed"));
    }
    let v: serde_json::Value = serde_json::from_str(&stdout).context("the builder's sync report")?;
    Ok(Synced {
        // Only a commit git there really has: the next bundle starts from it.
        sha: if v["git"].as_bool().unwrap_or(false) { v["commit"].as_str().unwrap_or("").to_owned() } else { String::new() },
        changed: v["changed"].as_u64().unwrap_or(0) as usize,
        sent: v["received"].as_i64().unwrap_or(0),
    })
}

/// A builder sends a path of the job back to the server, where `flow remote` fetches it.
async fn daemon_fetch(state: &crate::web::AppState, server: &BuildServer, job: &Job, path: &str) -> anyhow::Result<()> {
    check_fetch(path)?;
    let base = work_dir(server, &job.project);
    let from = match path.strip_prefix("target/") {
        Some(rest) => format!("{base}/target/{rest}"),
        None => format!("{base}/work/{path}"),
    };
    let name = crate::web::builders::result_name(path);
    let (jid, n2) = (job.id, name.clone());
    let (code, _, stderr) = crate::builders::hub()
        .ask(&server.name, |id| crate::builders::Request::Upload { id, job: jid, from, name: n2 }, Duration::from_secs(1800))
        .await?;
    if code != 0 {
        bail!("{}", stderr.trim().lines().last().unwrap_or("the upload failed"));
    }
    state
        .db
        .call(move |c| {
            let list: String = c.query_row("SELECT artifacts FROM remote_jobs WHERE id = ?1", [jid], |r| r.get(0))?;
            let mut v: Vec<String> = serde_json::from_str(&list).unwrap_or_default();
            if !v.contains(&name) {
                v.push(name);
            }
            c.execute("UPDATE remote_jobs SET artifacts = ?2 WHERE id = ?1", params![jid, serde_json::to_string(&v)?])?;
            Ok(())
        })
        .await?;
    Ok(())
}

/// Copies a path back from the work folder (or its target folder) into the checkout.
async fn fetch(data_dir: &Path, server: &BuildServer, job: &Job, path: &str) -> anyhow::Result<()> {
    check_fetch(path)?;
    let base = work_dir(server, &job.project);
    let from = match path.strip_prefix("target/") {
        Some(rest) => format!("{base}/target/{rest}"),
        None => format!("{base}/work/{path}"),
    };
    let to = Path::new(&job.dir).join(path);
    if let Some(p) = to.parent() {
        tokio::fs::create_dir_all(p).await?;
    }
    local(
        &job.dir,
        "rsync",
        &["-a", "--delete", &format!("{}:{from}", server.ssh), &to.to_string_lossy()],
        &ssh_command(data_dir, server),
        None,
    )
    .await?;
    Ok(())
}

async fn finish(state: &crate::web::AppState, job: &Job, st: &str, reason: &str, code: Option<i64>, out: Option<&Out>) {
    let (id, s, r) = (job.id, st.to_owned(), reason.to_owned());
    let (project_id, server, command) = (job.project_id, job.server.clone(), job.command.join(" "));
    let _ = state
        .db
        .call(move |c| {
            let now = now_ms();
            c.execute(
                "UPDATE remote_jobs SET state = ?2, reason = ?3, exit_code = ?4, finished_at = ?5 WHERE id = ?1 AND state != 'cancelled'",
                params![id, s, r, code, now],
            )?;
            // Its timing, on the one task in doing (or the one step under way), when that's clear.
            let started: Option<i64> = c.query_row("SELECT started_at FROM remote_jobs WHERE id = ?1", [id], |r| r.get(0))?;
            if let (Some(started), "done" | "failed") = (started, s.as_str()) {
                let p = crate::projects::get_by_id(c, project_id)?;
                if let Ok(on) = crate::times::resolve_on(c, project_id, &p.slug, None) {
                    let what: String = command.chars().take(200).collect();
                    crate::times::add(
                        c,
                        project_id,
                        &on,
                        &crate::times::NewTiming { what: &what, place: &server, wall_ms: now - started, cpu_ms: None, peak_mb: None, exit_code: code, note: "" },
                        "agentflow",
                    )?;
                }
            }
            Ok(())
        })
        .await;
    if let Some(out) = out {
        log_finish(state, job, st, code, out).await;
    }
}

/// The job's entry in the Log: its result, and the lines worth seeing (the start, warnings and
/// errors, the end); every line is an entry of its own too.
async fn log_finish(state: &crate::web::AppState, job: &Job, st: &str, code: Option<i64>, out: &Out) {
    let started = job.started_at.unwrap_or(job.created_at);
    let took = {
        let s = (now_ms() - started) / 1000;
        if s >= 3600 {
            format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
        } else {
            format!("{}:{:02}", s / 60, s % 60)
        }
    };
    let result = match (st, code) {
        ("done", _) => "ok".to_owned(),
        ("cancelled", _) => "cancelled".to_owned(),
        (_, Some(c)) if c != 255 => format!("failed (exit {c})"),
        _ => "failed".to_owned(),
    };
    let mut numbered: Vec<(i64, String)> = out.first.clone();
    numbered.extend(out.highlights.iter().cloned());
    numbered.extend(out.last.iter().cloned());
    numbered.sort_by_key(|(n, _)| *n);
    numbered.dedup_by_key(|(n, _)| *n);
    let lines: Vec<String> = numbered.into_iter().map(|(_, l)| l).collect();
    let e = crate::activity::Entry {
        actor_kind: "project".into(),
        actor: job.project.clone(),
        project: Some(job.project.clone()),
        via: crate::activity::Via::Build,
        action: crate::activity::Action::Run,
        summary: format!(
            "job #{} on {}: {} · {result} · {took} · {} lines, {} warnings, {} errors",
            job.id,
            job.server,
            job.command_line(),
            out.lines,
            out.warnings,
            out.errors
        ),
        target: Some(job.target()),
        status: None,
        duration_ms: Some(now_ms() - started),
        ip: None,
        detail: Some(
            serde_json::json!({"job": job.id, "lines": lines.iter().map(|l| crate::activity::redact(l)).collect::<Vec<_>>()}),
        ),
        severity: if st == "done" || st == "cancelled" {
            if out.warnings > 0 { "warn" } else { "info" }
        } else {
            "error"
        },
    };
    crate::activity::write(&state.db, e).await;
}

/// Deletes work folders unused for `keep_days`.
async fn cleanup(state: &crate::web::AppState, server: &BuildServer) -> anyhow::Result<()> {
    let script = format!(
        "cd \"$HOME\"/{root} 2>/dev/null || exit 0\nfor d in */; do d=${{d%/}}; [ -f \"$d/.used\" ] || continue; if [ -n \"$(find \"$d/.used\" -mtime +{days})\" ]; then rm -rf -- \"$d\"; echo \"$d\"; fi; done\n",
        root = sh_quote(&server.root),
        days = server.keep_days
    );
    let removed = run_script(&state.config.data_dir(), server, &script, Duration::from_secs(600)).await?;
    for d in removed.lines().filter(|l| !l.is_empty()) {
        let e = crate::activity::Entry::new(
            &crate::auth::Actor::System("build servers"),
            crate::activity::Via::Build,
            crate::activity::Action::System,
            format!("deleted {d}'s work folder on {} (unused for {} days)", server.name, server.keep_days),
        );
        crate::activity::write(&state.db, e).await;
    }
    Ok(())
}

/// The size of each project's work folder there, in GB (for the Machine page).
pub async fn sizes(data_dir: &Path, server: &BuildServer) -> anyhow::Result<Vec<(String, f64)>> {
    let script = format!("cd \"$HOME\"/{} 2>/dev/null && du -sk -- */ .sccache 2>/dev/null\n", sh_quote(&server.root));
    let text = run_script(data_dir, server, &script, Duration::from_secs(60)).await?;
    Ok(text
        .lines()
        .filter_map(|l| {
            let (kb, name) = l.split_once('\t')?;
            Some((name.trim_end_matches('/').to_owned(), kb.trim().parse::<f64>().ok()? / 1048576.0))
        })
        .collect())
}

/// Reads a job's output from `after` (bytes): the text, and where to go on from.
pub fn read_output(data_dir: &Path, id: i64, after: u64, max: u64) -> (String, u64) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(log_path(data_dir, id)) else { return (String::new(), after) };
    if f.seek(SeekFrom::Start(after)).is_err() {
        return (String::new(), after);
    }
    let mut buf = Vec::new();
    let _ = f.take(max).read_to_end(&mut buf);
    // Only whole lines.
    let end = buf.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    buf.truncate(end);
    (String::from_utf8_lossy(&buf).into_owned(), after + end as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_split_cpu_and_memory_between_the_owner_and_builds() {
        let text = |stat: &str, slice: &str| {
            format!(
                "stat cpu {stat}\nnproc 32\nmem MemTotal:       96418660 kB\nmem MemAvailable:   49692564 kB\nslice {slice} 22548578304\nmodel  AMD Ryzen 9 9950X3D 16-Core Processor\ngpu NVIDIA GeForce RTX 5090, 4\ndisk /dev/mapper/x 3900000000 3270000000 630000000 84% /home\n"
            )
        };
        let (first, c) = parse_probe(&text("1000 0 1000 8000 0 0 0 0", "0"), None, 0);
        assert_eq!(first.busy, 0.0, "the first probe can't know yet");
        assert_eq!(first.model, "AMD Ryzen 9 9950X3D 16-Core Processor");
        // 10 s later: 50 % busy over all 32 threads, of which the builds took 100 s of CPU (10 threads).
        let (p, _) = parse_probe(&text("6000 0 1000 13000 0 0 0 0", "100000000"), Some(c), 10_000);
        assert!((p.busy - 16.0).abs() < 0.01, "{}", p.busy);
        assert!((p.builds - 10.0).abs() < 0.01, "{}", p.builds);
        assert!((p.yours() - 6.0).abs() < 0.01);
        assert!((p.free_threads() - 16.0).abs() < 0.01);
        assert!((p.mem_total_gb - 91.95).abs() < 0.1 && (p.mem_builds_gb - 21.0).abs() < 0.01);
        assert_eq!((p.gpu_name.as_str(), p.gpu_util), ("NVIDIA GeForce RTX 5090", 4.0));
        assert!(p.disk_free_gb > 600.0);
    }

    #[test]
    fn quoting_and_paths() {
        assert_eq!(sh_quote("cargo"), "cargo");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("$(rm -rf ~)"), "'$(rm -rf ~)'");
        assert!(check_fetch("target/release/app").is_ok());
        for bad in ["", "/etc/passwd", "../x", "a/../../b", "-rf", "a//b", "a b"] {
            assert!(check_fetch(bad).is_err(), "{bad:?}");
        }
        assert_eq!(line_severity("error[E0425]: cannot find value"), "error");
        assert_eq!(line_severity("warning: unused variable"), "warn");
        assert_eq!(line_severity("   Compiling foo v0.1.0"), "info");
    }

    #[test]
    fn a_job_waits_for_room_and_its_project() {
        let server = BuildServer { name: "hq".into(), ssh: "hq".into(), ..Default::default() };
        let job = |id, project_id| Job {
            id,
            server: "hq".into(),
            project_id,
            project: "p".into(),
            actor: "p".into(),
            dir: "/x".into(),
            subdir: String::new(),
            command: vec!["cargo".into()],
            fetch: vec![],
            gpu: false,
            state: "queued".into(),
            reason: String::new(),
            exit_code: None,
            commit_sha: String::new(),
            dirty: 0,
            threads: None,
            sent_bytes: None,
            lines: 0,
            warnings: 0,
            errors: 0,
            created_at: 0,
            started_at: None,
            finished_at: None,
            manifest: String::new(),
            bundle: String::new(),
            setup: false,
            artifacts: Vec::new(),
        };
        let mut live = ServerLive::default();
        assert!(wait_reason(&server, &live, &job(1, 1), &[]).unwrap().contains("offline"));
        live.probe = Probe {
            online: true,
            threads: 32.0,
            busy: 6.0,
            mem_total_gb: 94.0,
            mem_free_gb: 50.0,
            ..Default::default()
        };
        assert_eq!(wait_reason(&server, &live, &job(1, 1), &[]), None);
        assert!(wait_reason(&server, &live, &job(2, 1), &[job(1, 1)]).unwrap().contains("this project"));
        assert_eq!(wait_reason(&server, &live, &job(2, 2), &[job(1, 1)]), None, "another project fits too");
        live.probe.busy = 28.0;
        assert!(wait_reason(&server, &live, &job(2, 2), &[]).unwrap().contains("free threads"));
        live.probe.busy = 6.0;
        live.probe.mem_free_gb = 12.0;
        assert!(wait_reason(&server, &live, &job(2, 2), &[]).unwrap().contains("memory"));
        live.probe.mem_free_gb = 50.0;
        live.paused = true;
        assert!(wait_reason(&server, &live, &job(2, 2), &[]).unwrap().contains("paused"));
    }
}
