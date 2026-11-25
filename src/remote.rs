//! Builders: agents run builds and tests on a stronger machine with
//! `flow remote --name "<what it is>" -- <command>`.
//! A builder connects itself to the server (`agentflow-builder`, see `crate::builders`); SSH
//! builders are gone (the owner, 2026-11-17: "can we remove SSH builders overall? like even the
//! option to have one"). AgentFlow brings the project's work folder there to the agent's snapshot,
//! queues and starts the job when the machine has room, streams its output to the agent, the job's
//! page and the Log, and sends results back.
//!
//! The owner, 2026-11-11: "lets make sure cache is used effectively and as little as possible is
//! rebuilt or moved over the network all the time" and "lets make it so the cap is dynamic, based
//! on whether other processes are using it right now or not. we can also build multiple things if
//! there is enough processing power".
//!
//! - **Little over the network:** each project has one work folder there; only files the builder
//!   doesn't have travel (by content), and only files that changed are rewritten, so cargo rebuilds
//!   exactly what changed. The target folder stays next to it between jobs.
//! - **Dynamic limits:** a job runs in a systemd unit of its own, at low CPU and IO weight and
//!   niceness, so it gets what the owner isn't using. Another job starts while enough threads and
//!   memory are free; when memory runs short, the newest job is frozen until there's room again.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use tokio::io::AsyncWriteExt;

use crate::config::BuildServer;
use crate::util::now_ms;

// ---------------------------------------------------------------------------------------------
// Scripts

/// Runs a shell script on a builder (with `sh`, in its home) and returns its output.
pub async fn run_script(
    _data_dir: &Path,
    server: &BuildServer,
    script: &str,
    timeout: Duration,
) -> anyhow::Result<String> {
    let script = script.to_owned();
    let (code, stdout, stderr) = crate::builders::hub()
        .ask(
            &server.name,
            |id| crate::builders::Request::Sh { id, script, stream: false, timeout_s: timeout.as_secs() },
            timeout + Duration::from_secs(5),
        )
        .await?;
    if code != 0 {
        bail!("{}", stderr.trim().lines().last().unwrap_or("failed"));
    }
    Ok(stdout)
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
    /// When it was probed last, whether it answered or not.
    pub at: i64,
    /// When it last answered: an offline builder's "last seen" (0: not since AgentFlow started).
    pub seen: i64,
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
    let mut p = Probe { at, seen: at, online: true, ..Default::default() };
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
}

#[derive(Default)]
pub struct Live {
    /// The build servers, from the database (the Settings page changes them).
    pub list: Vec<BuildServer>,
    pub servers: HashMap<String, ServerLive>,
    /// Running jobs, to stop them.
    running: HashMap<i64, tokio::sync::oneshot::Sender<()>>,
    /// When a page last asked: probes run every few seconds while someone looks or jobs run.
    pub watched_at: i64,
    /// When each job's `flow remote` last asked for its output: a job nobody follows any more is
    /// cancelled (see ORPHANED_AFTER).
    pub followed: HashMap<i64, i64>,
}

/// A job whose `flow remote` hasn't asked for its output this long is cancelled: its client is
/// gone (killed, timed out) and nobody gets its result (the owner, 2026-11-17: "yes, jobs like
/// this should be cancelled", after a fuzz job ran on for 45 minutes with its client killed and
/// held the builder's memory). Longer than `flow remote` waits out an AgentFlow restart (2 min).
pub const ORPHANED_AFTER_MS: i64 = 10 * 60_000;

/// Jobs among `active` nobody has followed for ORPHANED_AFTER_MS. A job seen for the first time
/// (new, or after a restart) starts its clock now; jobs no longer active are forgotten.
pub fn orphans(followed: &mut HashMap<i64, i64>, active: &[i64], now: i64) -> Vec<i64> {
    followed.retain(|id, _| active.contains(id));
    active.iter().filter(|id| now - *followed.entry(**id).or_insert(now) > ORPHANED_AFTER_MS).copied().collect()
}

pub type Shared = Arc<Mutex<Live>>;

// ---------------------------------------------------------------------------------------------
// Jobs

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: i64,
    /// What it is, in a few words ("Tests for PR #170"); '' for jobs from before names (D-275).
    pub name: String,
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
    /// Its work folder and target on the builder: 0 is the project's first.
    pub slot: i64,
}

impl Job {
    pub fn command_line(&self) -> String {
        self.command.iter().map(|w| sh_quote(w)).collect::<Vec<_>>().join(" ")
    }
    pub fn is_finished(&self) -> bool {
        matches!(self.state.as_str(), "done" | "failed" | "cancelled")
    }
    /// Its title: its name, or its command for a job from before names.
    pub fn title(&self) -> String {
        if self.name.is_empty() { self.command_line() } else { self.name.clone() }
    }
    pub fn target(&self) -> String {
        format!("job-{}", self.id)
    }
}

const JOB_COLUMNS: &str =
    "j.id, j.server, j.project_id, p.slug, j.actor, j.dir, j.subdir, j.command, j.fetch, j.gpu, j.state,
     j.reason, j.exit_code, j.commit_sha, j.dirty, j.threads, j.sent_bytes, j.lines, j.warnings, j.errors, j.created_at,
     j.started_at, j.finished_at, j.manifest, j.bundle, j.setup, j.artifacts, j.slot, j.name";

fn job_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    let list = |s: String| serde_json::from_str::<Vec<String>>(&s).unwrap_or_default();
    Ok(Job {
        id: r.get(0)?,
        name: r.get(28)?,
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
        slot: r.get(27)?,
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

pub fn in_state(conn: &Connection, server: &str, states: &[&str]) -> anyhow::Result<Vec<Job>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {JOB_COLUMNS} FROM remote_jobs j JOIN projects p ON p.id = j.project_id
         WHERE j.server = ?1 AND j.state IN (SELECT value FROM json_each(?2)) ORDER BY j.id"
    ))?;
    let rows = stmt.query_map(params![server, serde_json::to_string(states)?], job_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Which of a build server's jobs its page lists (D-277).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Show {
    All,
    Failed,
    /// Not over yet: running, paused or waiting.
    Running,
}

impl Show {
    pub fn parse(s: &str) -> Self {
        match s {
            "failed" => Show::Failed,
            "running" => Show::Running,
            _ => Show::All,
        }
    }

    /// Its value in the page's address (`?show=`), "" for all.
    pub fn key(self) -> &'static str {
        match self {
            Show::All => "",
            Show::Failed => "failed",
            Show::Running => "running",
        }
    }

    fn sql(self) -> &'static str {
        match self {
            Show::All => "1",
            Show::Failed => "j.state = 'failed'",
            Show::Running => "j.state IN ('running', 'paused', 'queued')",
        }
    }
}

/// A build server's jobs, counted: in all, failed and not over yet (the page's filters), the
/// first one's time, those since `today` and how many of them failed, and when the last finished.
#[derive(Debug, Clone, Default)]
pub struct Counts {
    pub all: i64,
    pub failed: i64,
    pub running: i64,
    pub first: Option<i64>,
    pub today: i64,
    pub today_failed: i64,
    pub last_finished: Option<i64>,
}

impl Counts {
    /// How many jobs `show` lists.
    pub fn of(&self, show: Show) -> i64 {
        match show {
            Show::All => self.all,
            Show::Failed => self.failed,
            Show::Running => self.running,
        }
    }
}

pub fn counts(conn: &Connection, server: &str, today: i64) -> anyhow::Result<Counts> {
    Ok(conn.query_row(
        "SELECT count(*), coalesce(sum(state = 'failed'), 0), coalesce(sum(state IN ('running', 'paused', 'queued')), 0),
                min(created_at), coalesce(sum(created_at >= ?2), 0), coalesce(sum(created_at >= ?2 AND state = 'failed'), 0),
                max(finished_at)
         FROM remote_jobs WHERE server = ?1",
        params![server, today],
        |r| {
            Ok(Counts {
                all: r.get(0)?,
                failed: r.get(1)?,
                running: r.get(2)?,
                first: r.get(3)?,
                today: r.get(4)?,
                today_failed: r.get(5)?,
                last_finished: r.get(6)?,
            })
        },
    )?)
}

/// One page of a build server's jobs, the newest first: `limit` of those `show` lists, from `offset`.
pub fn history(conn: &Connection, server: &str, show: Show, limit: i64, offset: i64) -> anyhow::Result<Vec<Job>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {JOB_COLUMNS} FROM remote_jobs j JOIN projects p ON p.id = j.project_id
         WHERE j.server = ?1 AND {} ORDER BY j.id DESC LIMIT ?2 OFFSET ?3",
        show.sql()
    ))?;
    let rows = stmt.query_map(params![server, limit, offset], job_from_row)?;
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
    /// What it is, in a few words; required (D-275).
    pub name: String,
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
    let name = j.name.trim();
    if name.is_empty() || name.chars().count() > 120 || name.contains('\n') {
        bail!("give the job a name, a few words on one line: flow remote --name \"Tests for PR #170\" -- …");
    }
    for f in &j.fetch {
        check_fetch(f)?;
    }
    conn.execute(
        "INSERT INTO remote_jobs (server, project_id, actor, dir, subdir, command, fetch, gpu, state, reason, created_at,
                                  manifest, bundle, setup, name)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'queued', 'waiting for its turn', ?9, ?10, ?11, ?12, ?13)",
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
            j.setup,
            name
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Jobs that were running when AgentFlow stopped: nobody waits for their output any more, so
/// they're over.
pub fn fail_orphans(conn: &Connection) -> anyhow::Result<Vec<(Job, u64)>> {
    // Those whose request on the builder is known run on: the scheduler follows them again.
    let keep: Vec<(Job, u64)> = in_states(conn, &["running", "paused"])?
        .into_iter()
        .filter_map(|j| {
            let rid: Option<i64> =
                conn.query_row("SELECT req_id FROM remote_jobs WHERE id = ?1", [j.id], |r| r.get(0)).ok().flatten();
            rid.map(|r| (j, r as u64))
        })
        .collect();
    conn.execute(
        "UPDATE remote_jobs SET state = 'failed', reason = 'AgentFlow restarted while it ran', finished_at = ?1
         WHERE state IN ('running', 'paused') AND req_id IS NULL",
        [now_ms()],
    )?;
    Ok(keep)
}

fn in_states(conn: &Connection, states: &[&str]) -> anyhow::Result<Vec<Job>> {
    let mut out = Vec::new();
    for s in load_servers(conn)? {
        out.extend(in_state(conn, &s.name, states)?);
    }
    Ok(out)
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
    let mine = running.iter().filter(|r| r.project_id == job.project_id).count() as i64;
    if mine >= server.slots.max(1) {
        return Some(if server.slots <= 1 {
            "another job of this project is running".into()
        } else {
            format!("{mine} jobs of this project are running (at most {} at once)", server.slots)
        });
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
        Ok(list) => state.remote.lock().unwrap_or_else(|e| e.into_inner()).list = list,
        Err(e) => tracing::warn!("build servers: {e:#}"),
    }
}

pub fn load_servers(conn: &Connection) -> anyhow::Result<Vec<BuildServer>> {
    let mut stmt = conn.prepare(
        "SELECT name, root, min_threads, min_memory_gb, reserve_memory_gb, keep_days, slots
         FROM build_servers ORDER BY created_at, name",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(BuildServer {
            name: r.get(0)?,
            root: r.get(1)?,
            min_threads: r.get(2)?,
            min_memory_gb: r.get(3)?,
            reserve_memory_gb: r.get(4)?,
            keep_days: r.get(5)?,
            slots: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// A name, a work folder and limits AgentFlow accepts.
pub fn check_server(b: &BuildServer) -> anyhow::Result<()> {
    let simple =
        |s: &str| !s.is_empty() && s.len() <= 64 && s.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c));
    if !simple(&b.name) {
        bail!("the name is letters, digits, - _ . (e.g. headquarters)");
    }
    if !b.root.split('/').all(|p| simple(p) && p != "." && p != "..") {
        bail!("the work folder is a folder in its home, like agentflow");
    }
    if !(b.min_threads >= 0.0 && b.min_memory_gb >= 0.0 && b.reserve_memory_gb >= 0.0 && b.keep_days >= 1) {
        bail!("the limits can't be negative, and folders are kept at least a day");
    }
    if !(1..=8).contains(&b.slots) {
        bail!("jobs of one project at once: 1 to 8");
    }
    Ok(())
}

/// The builder whose name is this one in any case, as it's written.
pub fn same_name(conn: &Connection, name: &str) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row("SELECT name FROM build_servers WHERE lower(name) = lower(?1) LIMIT 1", [name], |r| r.get(0))
        .optional()?)
}

/// Whether a builder has a token it can connect with.
pub fn has_token(conn: &Connection, name: &str) -> anyhow::Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM tokens WHERE kind = 'builder' AND name = ?1 AND revoked_at IS NULL)",
        [name],
        |r| r.get(0),
    )?)
}

/// A builder that connects itself (`flow builder add`). One without a token yet (a former SSH
/// builder) is taken over: its projects and job history stay.
pub fn add_daemon(conn: &Connection, b: &BuildServer) -> anyhow::Result<()> {
    check_server(b)?;
    let n = conn.execute(
        "INSERT OR IGNORE INTO build_servers (name, ssh, key_file, root, min_threads, min_memory_gb, reserve_memory_gb, keep_days, created_at, kind)
         VALUES (?1, '', '', ?2, ?3, ?4, ?5, ?6, ?7, 'daemon')",
        params![b.name, b.root, b.min_threads, b.min_memory_gb, b.reserve_memory_gb, b.keep_days, now_ms()],
    )?;
    if n == 0 && has_token(conn, &b.name)? {
        bail!("a builder called {} exists already; flow builder edit or remove it", b.name);
    }
    Ok(())
}

pub fn set_limits(conn: &Connection, name: &str, b: &BuildServer) -> anyhow::Result<()> {
    check_server(b)?;
    conn.execute(
        "UPDATE build_servers SET min_threads = ?2, min_memory_gb = ?3, reserve_memory_gb = ?4, keep_days = ?5, slots = ?6 WHERE name = ?1",
        params![name, b.min_threads, b.min_memory_gb, b.reserve_memory_gb, b.keep_days, b.slots],
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
    conn.execute(
        "UPDATE tokens SET revoked_at = ?2 WHERE kind = 'builder' AND name = ?1 AND revoked_at IS NULL",
        params![name, now_ms()],
    )?;
    Ok(())
}

/// Renames a builder: its projects, its job history and its token (a builder connects by name)
/// move with it.
pub fn rename_server(conn: &mut Connection, old: &str, new: &str) -> anyhow::Result<()> {
    let probe = BuildServer { name: new.to_owned(), ..Default::default() };
    check_server(&probe)?;
    let tx = conn.transaction()?;
    // A job in flight is tied to the builder's name: rename once it's idle.
    let busy: i64 = tx.query_row(
        "SELECT count(*) FROM remote_jobs WHERE server = ?1 AND state IN ('queued', 'running', 'paused')",
        [old],
        |r| r.get(0),
    )?;
    if busy > 0 {
        bail!("{old} has jobs waiting or running; rename it when they're done");
    }
    let taken: bool =
        tx.query_row("SELECT EXISTS (SELECT 1 FROM build_servers WHERE name = ?1)", [new], |r| r.get(0))?;
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

/// Where a project's agent builds, for `hello` and `flow remote` (the owner, 2026-11-17: agents
/// kept building locally while a builder was online): the picked builder and whether it's online,
/// or, with none picked, the builders that are online.
pub fn build_hint(conn: &Connection, project_id: i64) -> anyhow::Result<serde_json::Value> {
    let hub = crate::builders::hub();
    let online: Vec<String> = load_servers(conn)?.into_iter().map(|b| b.name).filter(|n| hub.is_online(n)).collect();
    Ok(match project_server(conn, project_id)? {
        Some(name) => {
            let up = hub.is_online(&name);
            serde_json::json!({
                "server": name,
                "online": up,
                "how": if up {
                    format!("Heavy builds and tests go to {name}: flow remote --name \"Release build\" [--fetch target/release/app] -- cargo build --release. Programs that must run on this machine (a different glibc) are built here.")
                } else {
                    format!("{name} is offline: build here (flow remote exits 75).")
                },
            })
        }
        None => serde_json::json!({
            "server": null,
            "online_builders": online,
            "how": if online.is_empty() {
                "No build server is picked for this project and none is online: build here.".to_owned()
            } else {
                format!("No build server is picked for this project (the owner picks one in its Settings), but {} online: flow remote --server {} --name \"…\" -- …", online.join(", "), online[0])
            },
        }),
    })
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
fn work_dir(server: &BuildServer, slug: &str, slot: i64) -> String {
    let root = server.root.trim_end_matches('/');
    // The first slot keeps the folder jobs always had.
    if slot <= 0 { format!("{root}/{slug}") } else { format!("{root}/{slug}.{slot}") }
}

/// A project's git objects on a builder, shared by its slots: a new slot needs no history of its
/// own, and a bundle's base is always there.
fn shared_git(server: &BuildServer, slug: &str) -> String {
    format!("{}/.git-{slug}", server.root.trim_end_matches('/'))
}

/// The first slot that no running job of the project holds.
fn free_slot(running: &[Job], job: &Job, slots: i64) -> i64 {
    (0..slots.max(1)).find(|s| !running.iter().any(|r| r.project_id == job.project_id && r.slot == *s)).unwrap_or(0)
}

/// Runs the scheduler: probes the servers, starts what fits, pauses and resumes for memory.
pub fn spawn(state: crate::web::AppState) {
    tokio::spawn(async move {
        match state.db.call(|c| fail_orphans(c)).await {
            Ok(keep) => {
                // Jobs that ran on while AgentFlow restarted: follow them again.
                let servers = state.db.call(|c| load_servers(c)).await.unwrap_or_default();
                for (job, rid) in keep {
                    if let Some(server) = servers.iter().find(|s| s.name == job.server).cloned() {
                        tracing::info!("following job #{} on {} again after the restart", job.id, server.name);
                        tokio::spawn(resume_job(state.clone(), server, job, rid));
                    }
                }
            }
            Err(e) => tracing::warn!("build servers: {e:#}"),
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
                for server in servers(&state).into_iter().filter(|s| crate::builders::hub().is_online(&s.name)) {
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
    let mut active = state.db.call(move |c| in_state(c, &name, &["queued", "running", "paused"])).await?;
    let now = now_ms();
    let gone = {
        let ids: Vec<i64> = active.iter().map(|j| j.id).collect();
        let mut live = state.remote.lock().unwrap_or_else(|e| e.into_inner());
        orphans(&mut live.followed, &ids, now)
    };
    for id in &gone {
        tracing::info!("job #{id}: nobody follows it any more, cancelling it");
        if let Err(e) = cancel(state, *id, "AgentFlow: its flow remote stopped following it 10 min ago").await {
            tracing::warn!("job #{id}: couldn't cancel it: {e:#}");
        }
    }
    active.retain(|j| !gone.contains(&j.id));
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
                let slot = free_slot(&busy, job, server.slots);
                state
                    .db
                    .call(move |c| {
                        c.execute(
                            "UPDATE remote_jobs SET state = 'running', reason = 'copying the changes', started_at = ?2, threads = ?3, slot = ?4 WHERE id = ?1",
                            params![id, now_ms(), threads, slot],
                        )?;
                        Ok(())
                    })
                    .await?;
                let mut job = job.clone();
                job.slot = slot;
                let (st, srv, probe) = (state.clone(), server.clone(), live_probe.clone());
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

/// Stops a job: the unit there, and the stream of its output.
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

/// What `sync` did.
struct Synced {
    sha: String,
    changed: usize,
    sent: i64,
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

    if !job.setup {
        out.line(&note(&format!(
            "copying {} to {}",
            if job.dir.is_empty() { "the snapshot" } else { &job.dir },
            server.name
        )))
        .await;
    }
    let synced = if job.setup {
        Ok(Synced { sha: String::new(), changed: 0, sent: 0 })
    } else {
        daemon_sync(&server, &job).await
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
    if !job.setup {
        out.line(&note(&format!(
            "at commit {} plus {} changed file{} ({} bytes sent)",
            synced.sha.get(..8).unwrap_or(""),
            synced.changed,
            if synced.changed == 1 { "" } else { "s" },
            synced.sent
        )))
        .await;
    }
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
    let base = work_dir(&server, &job.project, job.slot);
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

    let code = stream_daemon(&state, &server, &job, script, &mut out).await;
    finish_run(state, server, job, code, out).await;
}

async fn finish_run(state: crate::web::AppState, server: BuildServer, job: Job, code: Option<i64>, mut out: Out) {
    state.remote.lock().unwrap_or_else(|e| e.into_inner()).running.remove(&job.id);
    let id = job.id;
    let _ = state
        .db
        .call(move |c| {
            c.execute("UPDATE remote_jobs SET req_id = NULL WHERE id = ?1", [id])?;
            Ok(())
        })
        .await;
    if state.db.call(move |c| get(c, id)).await.ok().flatten().is_some_and(|j| j.state == "cancelled") {
        out.line(&note("cancelled")).await;
        flush(&state, &job, &mut out).await;
        log_finish(&state, &job, "cancelled", None, &out).await;
        return;
    }
    after_run(&state, &server, &job, code, &mut out).await;
}

/// Runs the job's script on a builder that connected itself. A dropped connection doesn't end
/// it: the builder sends the rest when it's back.
async fn stream_daemon(
    state: &crate::web::AppState,
    server: &BuildServer,
    job: &Job,
    script: String,
    out: &mut Out,
) -> Option<i64> {
    let (rid, events) = match crate::builders::hub().stream(&server.name, script) {
        Ok(x) => x,
        Err(e) => {
            out.line(&note(&format!("{e:#}"))).await;
            return None;
        }
    };
    // Kept, so a restarted AgentFlow follows the job again instead of failing it.
    let id = job.id;
    let _ = state
        .db
        .call(move |c| {
            c.execute("UPDATE remote_jobs SET req_id = ?2 WHERE id = ?1", params![id, rid as i64])?;
            Ok(())
        })
        .await;
    follow(state, server, job, rid, events, out).await
}

/// A job that ran on while AgentFlow restarted: its output goes on into the same file, and it
/// finishes as any other (the builder kept what it said meanwhile and sends it now).
async fn resume_job(state: crate::web::AppState, server: BuildServer, job: Job, rid: u64) {
    let path = log_path(&state.config.data_dir(), job.id);
    let before = tokio::fs::read_to_string(&path).await.unwrap_or_default().lines().count() as i64;
    let file = match tokio::fs::OpenOptions::new().append(true).create(true).open(&path).await {
        Ok(f) => f,
        Err(e) => {
            finish(&state, &job, "failed", &format!("can't write the output: {e}"), None, None).await;
            return;
        }
    };
    let mut out = Out {
        file,
        batch: Vec::new(),
        lines: before,
        warnings: 0,
        errors: 0,
        highlights: Vec::new(),
        first: Vec::new(),
        last: Default::default(),
    };
    out.line(&note("AgentFlow restarted; following the job again")).await;
    let events = crate::builders::hub().adopt(&server.name, rid);
    let code = follow(&state, &server, &job, rid, events, &mut out).await;
    finish_run(state, server, job, code, out).await;
}

/// The job's lines into its output until it ends.
async fn follow(
    state: &crate::web::AppState,
    server: &BuildServer,
    job: &Job,
    rid: u64,
    mut events: tokio::sync::mpsc::UnboundedReceiver<crate::builders::Event>,
    out: &mut Out,
) -> Option<i64> {
    let (tx, mut stop) = tokio::sync::oneshot::channel();
    state.remote.lock().unwrap_or_else(|e| e.into_inner()).running.insert(job.id, tx);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    // A builder gone for good (restarted, removed) never reports: give up after 10 minutes offline.
    let mut offline_since: Option<std::time::Instant> = None;
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
            _ = tick.tick() => {
                flush(state, job, out).await;
                if crate::builders::hub().is_online(&server.name) {
                    offline_since = None;
                } else if offline_since.get_or_insert_with(std::time::Instant::now).elapsed() > Duration::from_secs(600) {
                    out.line(&note(&format!("{} has been offline for 10 minutes; giving up on this job", server.name))).await;
                    break None;
                }
            }
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
async fn after_run(state: &crate::web::AppState, server: &BuildServer, job: &Job, code: Option<i64>, out: &mut Out) {
    let (state, server, job) = (state.clone(), server.clone(), job.clone());
    match code {
        Some(0) => {
            // Bring back what was asked for.
            let mut fetched = Vec::new();
            for f in &job.fetch {
                match daemon_fetch(&state, &server, &job, f).await {
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
    let work = format!("{}/work", work_dir(server, &job.project, job.slot));
    let shared = shared_git(server, &job.project);
    let seed = format!("{}/work", work_dir(server, &job.project, 0));
    let (manifest, bundle) = (job.manifest.clone(), job.bundle.clone());
    let (code, stdout, stderr) = crate::builders::hub()
        .ask(
            &server.name,
            |id| crate::builders::Request::Sync { id, work, manifest, bundle, commit: String::new(), shared, seed },
            Duration::from_secs(1800),
        )
        .await?;
    if code != 0 {
        bail!("{}", stderr.trim().lines().last().unwrap_or("the sync failed"));
    }
    let v: serde_json::Value = serde_json::from_str(&stdout).context("the builder's sync report")?;
    Ok(Synced {
        // Only a commit git there really has: the next bundle starts from it.
        sha: if v["git"].as_bool().unwrap_or(false) {
            v["commit"].as_str().unwrap_or("").to_owned()
        } else {
            String::new()
        },
        changed: v["changed"].as_u64().unwrap_or(0) as usize,
        sent: v["received"].as_i64().unwrap_or(0),
    })
}

/// A builder sends a path of the job back to the server, where `flow remote` fetches it.
async fn daemon_fetch(state: &crate::web::AppState, server: &BuildServer, job: &Job, path: &str) -> anyhow::Result<()> {
    check_fetch(path)?;
    let base = work_dir(server, &job.project, job.slot);
    let from = match path.strip_prefix("target/") {
        Some(rest) => format!("{base}/target/{rest}"),
        None => format!("{base}/work/{path}"),
    };
    let name = crate::web::builders::result_name(path);
    let (jid, n2) = (job.id, name.clone());
    let (code, _, stderr) = crate::builders::hub()
        .ask(
            &server.name,
            |id| crate::builders::Request::Upload { id, job: jid, from, name: n2 },
            Duration::from_secs(1800),
        )
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
    fn jobs_nobody_follows_are_orphans() {
        let mut f = HashMap::new();
        // First seen: the clock starts; followed recently: kept.
        assert!(orphans(&mut f, &[1, 2], 0).is_empty());
        f.insert(2, 9 * 60_000);
        assert_eq!(orphans(&mut f, &[1, 2], ORPHANED_AFTER_MS + 1), [1]);
        // Finished jobs are forgotten.
        orphans(&mut f, &[2], ORPHANED_AFTER_MS + 2);
        assert!(!f.contains_key(&1));
    }

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
        let server = BuildServer { name: "hq".into(), ..Default::default() };
        let job = |id, project_id| Job {
            name: String::new(),
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
            slot: 0,
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
        // With slots, several of one project run at once, each in a slot of its own.
        let wide = BuildServer { slots: 3, ..server.clone() };
        let mut first = job(1, 1);
        first.slot = 0;
        let mut second = job(2, 1);
        second.slot = 2;
        assert_eq!(wait_reason(&wide, &live, &job(3, 1), &[first.clone(), second.clone()]), None);
        assert_eq!(free_slot(&[first.clone(), second.clone()], &job(3, 1), 3), 1);
        let mut third = job(3, 1);
        third.slot = 1;
        assert!(wait_reason(&wide, &live, &job(4, 1), &[first, second, third]).unwrap().contains("at most 3"));
        assert_eq!(work_dir(&wide, "p", 0), "agentflow/p");
        assert_eq!(work_dir(&wide, "p", 2), "agentflow/p.2");
        live.paused = true;
        assert!(wait_reason(&server, &live, &job(2, 2), &[]).unwrap().contains("paused"));
    }
}
