//! `flow sync`: sends what's new in a project's Claude Code conversations to the server (see
//! `crate::web::sync`). Claude Code runs it as a hook after each reply (Stop), after each subagent
//! (SubagentStop) and when a session ends (SessionEnd), with the conversation's file on stdin. It
//! hands the work to a detached copy of itself and exits at once, so a session never waits, and it
//! prints nothing, so nothing lands in Claude's context. `flow sync --all` sends every earlier
//! conversation of the folder (`flow project setup` does that once).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::{Value, json};

use crate::cli::{Client, ClientArgs};
use crate::usage::{Read, folder_for, read_new};
use crate::util::expand_home;

/// Where Claude Code keeps conversations: one folder per working directory.
pub fn transcripts_root() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("projects"),
        _ => expand_home("~/.claude/projects"),
    }
}

/// A project folder's conversations folder.
pub fn folder_of(project_dir: &Path) -> PathBuf {
    transcripts_root().join(folder_for(&project_dir.display().to_string()))
}

/// The conversations of a folder: each session's file and its subagents' files.
pub fn conversations(project_dir: &Path) -> Vec<PathBuf> {
    let dir = folder_of(project_dir);
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else { return files };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "jsonl") {
            files.extend(with_subagents(&p));
        }
    }
    files.sort();
    files
}

/// A session's file and its subagents' (`<session>/subagents/*.jsonl` beside it).
fn with_subagents(session_file: &Path) -> Vec<PathBuf> {
    let mut v = vec![session_file.to_path_buf()];
    if let Some(stem) = session_file.file_stem() {
        let sub = session_file.with_file_name(stem).join("subagents");
        if let Ok(rd) = std::fs::read_dir(sub) {
            let mut subs: Vec<PathBuf> =
                rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect();
            subs.sort();
            v.extend(subs);
        }
    }
    v
}

/// How many sessions a folder has had (the hint `flow project setup` gives).
pub fn session_count(project_dir: &Path) -> (usize, Option<std::time::SystemTime>) {
    let dir = folder_of(project_dir);
    let mut n = 0;
    let mut latest = None;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.path().extension().is_some_and(|x| x == "jsonl") {
                n += 1;
                let m = e.metadata().and_then(|m| m.modified()).ok();
                if m > latest {
                    latest = m;
                }
            }
        }
    }
    (n, latest)
}

/// Sends what's new in these files. Returns the calls the server didn't have yet. A file that
/// fails doesn't hold up the others (the next sync tries it again from where the server stopped).
pub fn send(client: &Client, files: &[PathBuf]) -> anyhow::Result<usize> {
    let root = transcripts_root();
    let machine = crate::connect::machine_name();
    let mut calls = 0;
    let mut failed = Vec::new();
    for path in files {
        match send_file(client, &root, &format!("{machine}:{}", path.display()), path) {
            Ok(n) => calls += n,
            Err(e) => {
                eprintln!("flow sync: {}: {e:#}", path.display());
                failed.push(e);
            }
        }
    }
    match failed.into_iter().next() {
        Some(e) if calls == 0 && files.len() == 1 => Err(e),
        _ => Ok(calls),
    }
}

fn send_file(client: &Client, root: &Path, file: &str, path: &Path) -> anyhow::Result<usize> {
    let mut calls = 0;
    // Several rounds for a long file; each starts where the server stopped (asked again each
    // round, so a round refused because another sync got there first just moves on).
    for _ in 0..1000 {
        let st = client.post_json("/api/sync/state", &json!({"file": file}))?;
        let offset = st["offset"].as_u64().unwrap_or(0);
        let first_at = st["first_at"].as_i64();
        let Some(read) = read_new(root, path, offset, first_at)? else { break };
        if read.offset == offset {
            break; // only a line still being written
        }
        let from = if std::fs::metadata(path)?.len() < offset { 0 } else { offset };
        match client.post_json("/api/sync", &json!({"file": file, "from": from, "read": read_json(&read)?})) {
            Ok(got) => calls += got["calls"].as_u64().unwrap_or(0) as usize,
            Err(e) if format!("{e:#}").starts_with("409") => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(calls)
}

fn read_json(read: &Read) -> anyhow::Result<Value> {
    Ok(serde_json::to_value(read)?)
}

/// The conversation file Claude Code names on a hook's stdin, checked to be one of the project
/// folder's own: a project's token never carries another folder's sessions.
fn hook_files(input: &Value) -> anyhow::Result<(PathBuf, Vec<PathBuf>)> {
    let cwd = input["cwd"].as_str().map(PathBuf::from).or_else(|| std::env::current_dir().ok()).context("no folder")?;
    let project_dir = project_dir_from(&cwd);
    let mine = folder_of(&project_dir);
    let mut files = Vec::new();
    for key in ["transcript_path", "agent_transcript_path"] {
        if let Some(p) = input[key].as_str().filter(|p| !p.is_empty()) {
            let p = PathBuf::from(p);
            if p.starts_with(&mine) {
                files.extend(with_subagents(&p));
            }
        }
    }
    files.sort();
    files.dedup();
    Ok((project_dir, files))
}

/// The project folder a session works in: the closest folder upwards with AgentFlow's settings
/// (`.claude/settings.local.json` written by `flow project setup`), else the session's own.
fn project_dir_from(cwd: &Path) -> PathBuf {
    let mut d = Some(cwd);
    while let Some(dir) = d {
        if dir.join(".mcp.json").is_file() && dir.join(".claude/settings.local.json").is_file() {
            return dir.to_path_buf();
        }
        d = dir.parent();
    }
    cwd.to_path_buf()
}

/// Where a detached sync writes what went wrong (it has no terminal).
fn log_file() -> PathBuf {
    expand_home("~/.local/state/agentflow/sync.log")
}

fn note(line: &str) {
    use std::io::Write;
    let path = log_file();
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} {line}", crate::util::fmt_utc(crate::util::now_ms()));
    }
}

/// `flow sync`. From a hook (input on stdin): detach and return. With `--all`: every conversation
/// of the current folder, in the foreground. `files` (internal): the detached copy's work.
pub fn run(config_path: &Path, args: &ClientArgs, all: bool, files: Vec<PathBuf>) -> anyhow::Result<()> {
    if all {
        let dir = project_dir_from(&std::env::current_dir()?);
        let files = conversations(&dir);
        if files.is_empty() {
            println!("No earlier conversations in {}.", folder_of(&dir).display());
            return Ok(());
        }
        let client = Client::new(config_path, args)?.with_timeout(Duration::from_secs(300));
        let calls = send(&client, &files)?;
        println!("Sent {} conversation files ({calls} calls new to AgentFlow).", files.len());
        return Ok(());
    }
    if !files.is_empty() {
        // The detached copy: no terminal, so errors go to the log file.
        let client = match Client::new(config_path, args) {
            Ok(c) => c.with_timeout(Duration::from_secs(300)),
            Err(e) => {
                note(&format!("{e:#}"));
                return Ok(());
            }
        };
        if let Err(e) = send(&client, &files) {
            note(&format!("{}: {e:#}", files[0].display()));
        }
        return Ok(());
    }
    // From a hook: read what Claude Code sends, then hand the work over and return at once.
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin().take_limit(), &mut text)?;
    let input: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let (_, files) = match hook_files(&input) {
        Ok(f) => f,
        Err(e) => {
            note(&format!("{e:#}"));
            return Ok(());
        }
    };
    if files.is_empty() {
        return Ok(());
    }
    detach(args, &files)
}

trait TakeLimit {
    fn take_limit(self) -> std::io::Take<Self>
    where
        Self: Sized;
}

impl<R: std::io::Read> TakeLimit for R {
    /// A hook's input is a small JSON object.
    fn take_limit(self) -> std::io::Take<Self> {
        std::io::Read::take(self, 1024 * 1024)
    }
}

/// Starts `flow sync --file …` in its own session, without the hook's pipes, and returns.
fn detach(args: &ClientArgs, files: &[PathBuf]) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let me = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(me);
    cmd.arg("sync");
    if let Some(u) = &args.url {
        cmd.arg("--url").arg(u);
    }
    if let Some(t) = &args.token_file {
        cmd.arg("--token-file").arg(t);
    }
    for f in files {
        cmd.arg("--file").arg(f);
    }
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    // Its own session: Claude Code ending the hook's process group doesn't end the sync.
    unsafe {
        cmd.pre_exec(|| {
            rustix::process::setsid().map_err(std::io::Error::from)?;
            Ok(())
        });
    }
    match cmd.spawn() {
        Ok(_) => Ok(()),
        Err(e) => bail!("starting the sync: {e}"),
    }
}
