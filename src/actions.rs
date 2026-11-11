//! Buttons. The owner, 2026-11-07: "lets add buttons for the merges and tag releases. the agents
//! can add their own git / gh commands there so it stays flexible". An agent attaches commands to
//! a PR or release; the owner's press runs them on this machine and is their approval. Agents never
//! run them. Any command is allowed; the page always shows it before the press, and the press
//! carries a hash of what was shown, so the command can't change between looking and pressing.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::records::{self, Kind, Record};
use crate::util::{expand_home, now_ms};

const TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_OUTPUT: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct ActionRow {
    pub id: i64,
    pub record_id: i64,
    pub label: String,
    pub command: String,
    pub workdir: String,
    pub requires: Vec<String>,
    pub added_by: String,
    pub created_at: i64,
    pub state: String,
    pub pressed_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub exit_code: Option<i64>,
    pub output: String,
}

impl ActionRow {
    /// Short hash of exactly what the page showed: command and folder.
    pub fn hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.workdir.as_bytes());
        h.update([0]);
        h.update(self.command.as_bytes());
        h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect()
    }

    pub fn is_merge(&self) -> bool {
        self.label.to_lowercase().starts_with("merge")
    }
}

const COLUMNS: &str = "id, record_id, label, command, workdir, requires, added_by, created_at, state, pressed_at, finished_at, exit_code, output";

fn from_row(r: &Row) -> rusqlite::Result<ActionRow> {
    Ok(ActionRow {
        id: r.get(0)?,
        record_id: r.get(1)?,
        label: r.get(2)?,
        command: r.get(3)?,
        workdir: r.get(4)?,
        requires: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
        added_by: r.get(6)?,
        created_at: r.get(7)?,
        state: r.get(8)?,
        pressed_at: r.get(9)?,
        finished_at: r.get(10)?,
        exit_code: r.get(11)?,
        output: r.get(12)?,
    })
}

pub fn get(conn: &Connection, id: i64) -> anyhow::Result<Option<ActionRow>> {
    Ok(conn.query_row(&format!("SELECT {COLUMNS} FROM actions WHERE id = ?1"), [id], from_row).optional()?)
}

pub fn for_record(conn: &Connection, record_id: i64) -> anyhow::Result<Vec<ActionRow>> {
    let mut stmt = conn
        .prepare(&format!("SELECT {COLUMNS} FROM actions WHERE record_id = ?1 AND state != 'withdrawn' ORDER BY id"))?;
    let rows = stmt.query_map([record_id], from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Adds a button. `requires` lists records (PRs) that must be merged, or tasks done, first.
pub fn add(
    conn: &Connection,
    record: &Record,
    label: &str,
    command: &str,
    workdir: &str,
    requires: &[String],
    added_by: &str,
) -> anyhow::Result<i64> {
    if !matches!(record.kind, Kind::Pr | Kind::Release) {
        bail!("buttons belong to a PR or a release, not a {}", record.kind);
    }
    let label = label.trim();
    if label.is_empty() || label.chars().count() > 60 {
        bail!("label: a few words, e.g. \"Merge\" or \"Push tag v0.0.5-m4\"");
    }
    let command = command.trim();
    if command.is_empty() || command.len() > 8000 {
        bail!("command: the exact lines to run, at most 8000 characters");
    }
    // Check the required records exist now, so a typo doesn't leave a button that never unlocks.
    let mut resolved = Vec::new();
    for r in requires {
        resolved.push(records::find_str(conn, r, Some(&record.project))?.ref_id);
    }
    conn.execute(
        "INSERT INTO actions (record_id, label, command, workdir, requires, added_by, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![record.id, label, command, workdir.trim(), serde_json::to_string(&resolved)?, added_by, now_ms()],
    )?;
    let id = conn.last_insert_rowid();
    records::event(
        conn,
        Some(record.project_id),
        Some(record.id),
        "action.added",
        &format!("Button \"{label}\" added to {}", record.short_id()),
        added_by,
    )?;
    Ok(id)
}

pub fn withdraw(conn: &Connection, id: i64, by: &str) -> anyhow::Result<()> {
    let n =
        conn.execute("UPDATE actions SET state = 'withdrawn' WHERE id = ?1 AND state IN ('waiting', 'failed')", [id])?;
    if n == 0 {
        bail!("button {id} can't be withdrawn now (it ran, is running, or doesn't exist)");
    }
    let _ = by;
    Ok(())
}

/// Why the button is disabled, or Ok when it can be pressed.
pub fn enabled(conn: &Connection, a: &ActionRow) -> anyhow::Result<Result<(), String>> {
    match a.state.as_str() {
        "waiting" | "failed" => {}
        "running" => return Ok(Err("running now".into())),
        "done" => return Ok(Err("done".into())),
        s => return Ok(Err(s.into())),
    }
    for req in &a.requires {
        let r = records::find_str(conn, req, None)?;
        let ok = matches!(r.state.as_str(), "merged" | "done" | "tagged");
        if !ok {
            return Ok(Err(format!("unlocks once {} is merged", r.short_id())));
        }
    }
    let record = records::get(conn, a.record_id)?;
    if record.kind == Kind::Pr && a.is_merge() {
        if let Err(why) = crate::prs::merge_ready(&record) {
            return Ok(Err(why));
        }
        // A stacked PR merges after the one it's based on (or with "Merge up to here").
        if let Some(o) =
            crate::prs::merge_order(conn, record.project_id)?.into_iter().find(|o| o.record.id == record.id)
            && let Some(after) = o.after
        {
            return Ok(Err(format!("merge PR-{after} first")));
        }
    }
    Ok(Ok(()))
}

/// Marks an action running, if it may be pressed and the page showed this exact command.
pub fn claim(conn: &Connection, id: i64, shown_hash: &str) -> anyhow::Result<ActionRow> {
    let a = get(conn, id)?.context("no such button")?;
    if a.hash() != shown_hash {
        bail!("this button's command isn't the one the page showed; reload and look again");
    }
    if let Err(why) = enabled(conn, &a)? {
        bail!("can't run it now: {why}");
    }
    let n = conn.execute(
        "UPDATE actions SET state = 'running', pressed_at = ?1 WHERE id = ?2 AND state IN ('waiting', 'failed')",
        params![now_ms(), id],
    )?;
    if n == 0 {
        bail!("it was pressed a moment ago");
    }
    Ok(a)
}

pub struct RunResult {
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub output: String,
}

/// Runs the command with bash in its folder, with a 10-minute limit. Never called by an agent.
pub async fn execute(a: &ActionRow) -> RunResult {
    let dir: PathBuf = if a.workdir.is_empty() { crate::util::home_dir() } else { expand_home(&a.workdir) };
    let child = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(&a.command)
        .current_dir(&dir)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(TIMEOUT, child).await {
        Err(_) => RunResult { ok: false, exit_code: None, output: "Stopped after 10 minutes.".into() },
        Ok(Err(e)) => {
            RunResult { ok: false, exit_code: None, output: format!("Couldn't start it in {}: {e}", dir.display()) }
        }
        Ok(Ok(out)) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.trim().is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&err);
            }
            if text.len() > MAX_OUTPUT {
                let cut = text.char_indices().map(|(i, _)| i).take_while(|&i| i <= MAX_OUTPUT).last().unwrap_or(0);
                text.truncate(cut);
                text.push_str("\n[output cut at 64 KB]");
            }
            RunResult { ok: out.status.success(), exit_code: out.status.code(), output: crate::activity::redact(&text) }
        }
    }
}

/// Stores the result and tells the project's agent.
pub fn finish(conn: &Connection, a: &ActionRow, r: &RunResult) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE actions SET state = ?1, finished_at = ?2, exit_code = ?3, output = ?4 WHERE id = ?5",
        params![if r.ok { "done" } else { "failed" }, now_ms(), r.exit_code, r.output, a.id],
    )?;
    let record = records::get(conn, a.record_id)?;
    let (kind, what) = if r.ok {
        ("action.done", format!("Owner pressed \"{}\" on {}: done", a.label, record.short_id()))
    } else {
        (
            "action.failed",
            format!(
                "Owner pressed \"{}\" on {}: failed, exit {}",
                a.label,
                record.short_id(),
                r.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "none".into())
            ),
        )
    };
    records::event(conn, Some(record.project_id), Some(record.id), kind, &what, "owner")?;
    // A release whose tag command ran is tagged.
    if r.ok && record.kind == Kind::Release && a.label.to_lowercase().contains("tag") {
        records::update(
            conn,
            &record,
            "owner",
            records::Patch { state: Some("tagged".into()), quiet: true, ..Default::default() },
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::github::fake::pr;

    #[tokio::test]
    async fn buttons_run_once_and_only_as_shown() {
        let db = Db::open_memory();
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().display().to_string();
        let (a, task) = db
            .call(move |c| {
                let p = crate::projects::create(c, "v", "V", "t")?;
                let (r, _) = crate::prs::sync(c, &p, "o/r", &pr(48, "main", "b48"), "poller")?;
                let task = records::create(
                    c,
                    p.id,
                    "v",
                    records::NewRecord { kind: Some(Kind::Task), title: "t".into(), ..Default::default() },
                )?;
                assert!(add(c, &task, "Merge", "x", "", &[], "v").is_err(), "only PRs and releases");
                assert!(add(c, &r, "Merge", "x", "", &["T-9".into()], "v").is_err(), "unknown requirement");
                let id = add(c, &r, "Merge", "echo merged $PWD; echo warn >&2", &workdir, &["T-1".into()], "v")?;
                Ok((get(c, id)?.unwrap(), task))
            })
            .await
            .unwrap();

        // Not ready (agent, task) → can't claim; a wrong hash never can.
        let (id, hash) = (a.id, a.hash());
        let err = db.call(move |c| claim(c, id, &hash)).await.unwrap_err().to_string();
        assert!(err.contains("T-1 is merged") || err.contains("unlocks"), "{err}");
        db.call(move |c| {
            records::update(c, &task, "v", records::Patch { state: Some("done".into()), ..Default::default() })?;
            let r = records::find_str(c, "v/PR-48", None)?;
            crate::prs::set_ready(c, &r, true, "", "v")?;
            Ok(())
        })
        .await
        .unwrap();
        assert!(db.call(move |c| claim(c, id, "0000")).await.is_err());

        let hash = a.hash();
        let claimed = db.call(move |c| claim(c, id, &hash)).await.unwrap();
        let hash = a.hash();
        assert!(db.call(move |c| claim(c, id, &hash)).await.is_err(), "pressed twice");
        let result = execute(&claimed).await;
        assert!(result.ok);
        assert!(result.output.contains("merged") && result.output.contains("warn"));
        assert!(result.output.contains(&*dir.path().display().to_string()), "runs in its folder");
        db.call(move |c| finish(c, &claimed, &result)).await.unwrap();
        let done = db.call(move |c| get(c, id)).await.unwrap().unwrap();
        assert_eq!((done.state.as_str(), done.exit_code), ("done", Some(0)));
    }

    #[tokio::test]
    async fn failures_and_secrets() {
        let a = ActionRow {
            id: 1,
            record_id: 1,
            label: "Push tag".into(),
            command: format!("echo token {}; exit 3", crate::util::new_secret("af_prj_")),
            workdir: String::new(),
            requires: vec![],
            added_by: "v".into(),
            created_at: 0,
            state: "waiting".into(),
            pressed_at: None,
            finished_at: None,
            exit_code: None,
            output: String::new(),
        };
        let r = execute(&a).await;
        assert!(!r.ok);
        assert_eq!(r.exit_code, Some(3));
        assert!(r.output.contains("[redacted]") && !r.output.contains("af_prj_"));
    }
}
