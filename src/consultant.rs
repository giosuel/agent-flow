//! The consultant, run by the server (the owner, 2026-11-13: "the consultant should be managed by
//! the agentflow server. no setup, nothing. it just runs together with the server"): switched on
//! and off in Settings; a Claude Code session with Remote Control in tmux ("so i can access it
//! directly", "a link to the claude.ai remote control session", "no input field"), in its own folder
//! in the data folder, as the server's user, on Opus 5.5. Agents reach it with SendMessage as
//! before. Turning it on needs a claude.ai login of the server's user and fails with an error
//! otherwise.
//!
//! Its permissions: AgentFlow's tools are allowed; shell commands and edits ask, and the owner
//! answers in tmux or on claude.ai. Otherwise any agent could have it run commands as the
//! server's user by messaging it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use crate::auth::{self, TokenKind};

/// Its tmux session, and the name it has in Remote Control (agents message it by this name).
pub const TMUX: &str = "agentflow-consultant";
pub const NAME: &str = "AgentFlow Consultant";
pub const MODEL: &str = "claude-opus-5-5";

pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("consultant")
}

fn setting(c: &Connection, key: &str) -> anyhow::Result<Option<String>> {
    Ok(c.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).optional()?)
}

fn set_setting(c: &Connection, key: &str, value: &str) -> anyhow::Result<()> {
    c.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub fn is_on(c: &Connection) -> anyhow::Result<bool> {
    Ok(setting(c, "consultant.on")?.as_deref() == Some("1"))
}

pub fn set_on(c: &Connection, on: bool) -> anyhow::Result<()> {
    set_setting(c, "consultant.on", if on { "1" } else { "0" })
}

/// A conversation to continue on its first start (the session moved in from the consultant that
/// ran by itself before).
pub fn resume_first(c: &Connection) -> anyhow::Result<Option<String>> {
    setting(c, "consultant.resume")
}

pub fn set_resume_first(c: &Connection, session: &str) -> anyhow::Result<()> {
    set_setting(c, "consultant.resume", session)
}

/// Its one token: a new one each time it's set up; any other consultant token is revoked.
pub fn new_token(c: &mut Connection) -> anyhow::Result<String> {
    let tx = c.transaction()?;
    tx.execute("UPDATE tokens SET revoked_at = ?1 WHERE kind = 'admin' AND revoked_at IS NULL", [crate::util::now_ms()])?;
    let (_, secret) = auth::create_token(&tx, TokenKind::Admin, NAME, None)?;
    tx.commit()?;
    Ok(secret)
}

/// Is the server's user signed in to claude.ai (needed for Remote Control)?
pub async fn login_ok() -> anyhow::Result<()> {
    let out = tokio::time::timeout(Duration::from_secs(20), tokio::process::Command::new("claude").args(["auth", "status", "--json"]).kill_on_drop(true).output())
        .await
        .context("claude auth status took too long")?
        .context("Claude Code isn't installed for the server's user (claude)")?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    if v["loggedIn"].as_bool() != Some(true) || v["authMethod"].as_str() != Some("claude.ai") {
        bail!("the server's user isn't signed in to claude.ai with a subscription (Remote Control needs that). Sign it in once: claude auth login (as the server's user, e.g. sudo -u agentflow claude auth login)");
    }
    Ok(())
}

/// Its folder: AgentFlow's MCP server with its token, AgentFlow's tools allowed.
pub fn write_folder(data_dir: &Path, token: &str, public_url: &str, flow: &Path) -> anyhow::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let d = dir(data_dir);
    std::fs::create_dir_all(d.join(".claude"))?;
    let token_file = d.join("token");
    let tmp = d.join(".token.new");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    writeln!(f, "{token}")?;
    std::fs::rename(&tmp, &token_file)?;
    let mcp = json!({"mcpServers": {"agentflow": {
        "type": "http",
        "url": format!("{}/mcp", public_url.trim_end_matches('/')),
        "headersHelper": format!("{} mcp-headers --token-file {}", flow.display(), token_file.display()),
    }}});
    std::fs::write(d.join(".mcp.json"), serde_json::to_string_pretty(&mcp)? + "\n")?;
    let settings = json!({
        "enabledMcpjsonServers": ["agentflow"],
        "permissions": {"allow": ["mcp__agentflow"]},
        "env": {"AGENTFLOW_TOKEN_FILE": token_file.display().to_string(), "AGENTFLOW_URL": public_url.trim_end_matches('/')},
    });
    std::fs::write(d.join(".claude/settings.local.json"), serde_json::to_string_pretty(&settings)? + "\n")?;
    Ok(d)
}

pub async fn running() -> bool {
    tokio::process::Command::new("tmux")
        .args(["has-session", "-t", TMUX])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Starts it in tmux: the conversation moved in (once), else the folder's last one, else new.
pub async fn start(folder: &Path, resume: Option<&str>, has_history: bool) -> anyhow::Result<()> {
    let mut cmd = vec!["claude".to_owned()];
    match resume {
        Some(id) => cmd.extend(["--resume".into(), id.to_owned()]),
        None if has_history => cmd.push("--continue".into()),
        None => {}
    }
    // The name other sessions find it by (ListAgents, SendMessage) and its title on claude.ai.
    cmd.extend(["--name".into(), NAME.into(), "--remote-control".into(), NAME.into(), "--model".into(), MODEL.into()]);
    let shell = cmd.iter().map(|w| crate::remote::sh_quote(w)).collect::<Vec<_>>().join(" ");
    let out = tokio::process::Command::new("tmux")
        .args(["new-session", "-d", "-s", TMUX, "-x", "200", "-y", "50", "-c"])
        .arg(folder)
        .arg(shell)
        .output()
        .await
        .context("running tmux (is it installed for the server's user?)")?;
    if !out.status.success() {
        bail!("tmux: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

pub async fn stop() -> anyhow::Result<()> {
    let _ = tokio::process::Command::new("tmux").args(["kill-session", "-t", TMUX]).output().await?;
    Ok(())
}

/// Has it talked in this folder before (Claude Code keeps its conversations by folder)?
pub fn has_history(transcripts: &Path, folder: &Path) -> bool {
    let d = transcripts.join(crate::usage::folder_for(&folder.display().to_string()));
    std::fs::read_dir(d).map(|rd| rd.flatten().any(|e| e.path().extension().is_some_and(|x| x == "jsonl"))).unwrap_or(false)
}

/// Its newest claude.ai link (from its own conversations' Remote Control lines).
pub fn session_link(c: &Connection, folder: &Path) -> anyhow::Result<Option<String>> {
    let f = crate::usage::folder_for(&folder.display().to_string());
    Ok(c.query_row("SELECT url FROM remote_sessions WHERE folder = ?1 ORDER BY at DESC LIMIT 1", [f], |r| r.get(0)).optional()?)
}

/// Reads what's new in its conversations (the server reads no other transcripts when
/// `local_transcripts` is off).
pub fn ingest(c: &mut Connection, transcripts: &Path, folder: &Path) -> anyhow::Result<usize> {
    let d = transcripts.join(crate::usage::folder_for(&folder.display().to_string()));
    let mut calls = 0;
    for path in crate::usage::transcripts(&d) {
        let (offset, first_at) = crate::usage::file_state(c, &path)?;
        if let Some(read) = crate::usage::read_new(transcripts, &path, offset, first_at)? {
            calls += crate::usage::store(c, &read)?;
        }
    }
    Ok(calls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn one_token_and_a_folder_that_only_allows_agentflow() {
        let db = Db::open_memory();
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().to_owned();
        db.call_blocking(move |c| {
            let a = new_token(c)?;
            let b = new_token(c)?;
            assert!(auth::token_actor(c, &a, "x")?.is_none(), "the older one is revoked");
            assert!(auth::token_actor(c, &b, "x")?.is_some());
            let d = write_folder(&data, &b, "https://flow.example", Path::new("/usr/bin/flow"))?;
            let s = std::fs::read_to_string(d.join(".claude/settings.local.json"))?;
            assert!(s.contains("mcp__agentflow") && !s.contains("Bash"), "{s}");
            let m = std::fs::read_to_string(d.join(".mcp.json"))?;
            assert!(m.contains("mcp-headers --token-file") && !m.contains(&b), "the token stays in its file");
            set_on(c, true)?;
            assert!(is_on(c)?);
            Ok(())
        })
        .unwrap();
    }
}
