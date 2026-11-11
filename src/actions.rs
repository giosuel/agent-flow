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
    /// What pressing it means to AgentFlow: `merge` (a PR's merge, gated by merge order), `tag`
    /// (a release's tag, which marks it tagged), or `other`.
    pub kind: ButtonKind,
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
        self.kind == ButtonKind::Merge
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ButtonKind {
    Merge,
    Tag,
    Other,
}

impl ButtonKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ButtonKind::Merge => "merge",
            ButtonKind::Tag => "tag",
            ButtonKind::Other => "other",
        }
    }

    pub fn parse(s: &str) -> anyhow::Result<Self> {
        Ok(match s.trim() {
            "merge" => ButtonKind::Merge,
            "tag" => ButtonKind::Tag,
            "other" | "" => ButtonKind::Other,
            other => bail!("kind must be merge, tag or other, not {other:?}"),
        })
    }
}

const COLUMNS: &str = "id, record_id, label, kind, command, workdir, requires, added_by, created_at, state, pressed_at, finished_at, exit_code, output";

fn from_row(r: &Row) -> rusqlite::Result<ActionRow> {
    Ok(ActionRow {
        id: r.get(0)?,
        record_id: r.get(1)?,
        label: r.get(2)?,
        kind: ButtonKind::parse(&r.get::<_, String>(3)?).unwrap_or(ButtonKind::Other),
        command: r.get(4)?,
        workdir: r.get(5)?,
        requires: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or_default(),
        added_by: r.get(7)?,
        created_at: r.get(8)?,
        state: r.get(9)?,
        pressed_at: r.get(10)?,
        finished_at: r.get(11)?,
        exit_code: r.get(12)?,
        output: r.get(13)?,
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
/// A button as an agent describes it.
pub struct NewButton<'a> {
    pub label: &'a str,
    pub kind: ButtonKind,
    pub command: &'a str,
    pub workdir: &'a str,
    /// Records that must be merged (or done) first.
    pub requires: &'a [String],
}

pub fn add(conn: &Connection, record: &Record, b: &NewButton, added_by: &str) -> anyhow::Result<i64> {
    let NewButton { label, kind, command, workdir, requires } = *b;
    if !matches!(record.kind, Kind::Pr | Kind::Release) {
        bail!("buttons belong to a PR or a release, not a {}", record.kind);
    }
    match (kind, record.kind) {
        (ButtonKind::Merge, Kind::Release) => bail!("a merge button belongs on a PR"),
        (ButtonKind::Tag, Kind::Pr) => bail!("a tag button belongs on a release"),
        _ => {}
    }
    let label = label.trim();
    if label.is_empty() || label.chars().count() > 60 {
        bail!("label: a few words, e.g. \"Merge\" or \"Push tag v0.0.5-m4\"");
    }
    let command = command.trim();
    check_shown_whole(command)?;
    // What the owner reads must be what runs: no invisible or reordering characters.
    for (what, text) in [("label", label), ("command", command), ("workdir", workdir)] {
        if let Some(c) = text.chars().find(|&c| hidden_char(c)) {
            bail!("{what} contains an invisible character (U+{:04X}); write it in plain text", c as u32);
        }
    }
    // Check the required records exist now, so a typo doesn't leave a button that never unlocks.
    let mut resolved = Vec::new();
    for r in requires {
        resolved.push(records::find_str(conn, r, Some(&record.project))?.ref_id);
    }
    conn.execute(
        "INSERT INTO actions (record_id, label, kind, command, workdir, requires, added_by, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            record.id,
            label,
            kind.as_str(),
            command,
            workdir.trim(),
            serde_json::to_string(&resolved)?,
            added_by,
            now_ms()
        ],
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

/// After a restart, buttons that were running can't still be: mark them failed, so they can be
/// looked at, pressed again or withdrawn.
pub fn recover_interrupted(conn: &Connection) -> anyhow::Result<usize> {
    Ok(conn.execute(
        "UPDATE actions SET state = 'failed', finished_at = ?1,
            output = output || 'AgentFlow restarted while this ran. Check the result on GitHub before pressing it again.'
         WHERE state = 'running'",
        [now_ms()],
    )?)
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

/// Characters that don't show, or change how the rest shows: control codes (other than newline
/// and tab), bidi overrides and isolates, zero-width characters, soft hyphens, BOMs.
fn hidden_char(c: char) -> bool {
    (c.is_control() && c != '\n' && c != '\t')
        || matches!(c as u32, 0xAD | 0x061C | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB)
}

/// The confirmation dialog shows a command whole, so it must fit one: short enough, and without
/// blank stretches or runs of spaces that would push a line out of sight.
fn check_shown_whole(command: &str) -> anyhow::Result<()> {
    if command.is_empty() || command.chars().count() > 2000 || command.lines().count() > 40 {
        bail!("command: the exact lines to run, at most 40 lines and 2000 characters (the owner reads all of it)");
    }
    if command.contains("\n\n\n") || command.lines().any(|l| l.contains(&" ".repeat(24)) || l.contains("\t\t\t")) {
        bail!("command: no runs of blank lines or long runs of spaces; the confirmation shows it as written");
    }
    Ok(())
}

/// The text of the confirmation dialog: the command itself, whole, not just the button's name.
pub fn confirm_text(a: &ActionRow) -> String {
    let dir = if a.workdir.is_empty() { "~" } else { &a.workdir };
    format!("Run \"{}\" in {dir}?\n\n{}", a.label, a.command)
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
        if !records::is_finished(&r) {
            return Ok(Err(format!("unlocks once {} is {}", records::display_id(&r), records::finished_word(r.kind))));
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
            return Ok(Err(format!("merge #{after} first")));
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
    if r.ok && record.kind == Kind::Release && a.kind == ButtonKind::Tag {
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

    #[test]
    fn commands_fit_their_confirmation() {
        assert!(check_shown_whole("gh pr merge 12 --merge\n\ngit tag v1").is_ok());
        for hidden in [
            format!("echo ok\n\n\n\n{}rm -rf ~", ""),
            format!("echo ok{}; rm -rf ~", " ".repeat(200)),
            "x".repeat(2001),
            "echo\n".repeat(41),
        ] {
            assert!(check_shown_whole(&hidden).is_err(), "{hidden:?}");
        }
    }

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
                assert!(
                    add(
                        c,
                        &task,
                        &NewButton {
                            label: "Merge",
                            kind: ButtonKind::Merge,
                            command: "x",
                            workdir: "",
                            requires: &[]
                        },
                        "v"
                    )
                    .is_err(),
                    "only PRs and releases"
                );
                assert!(
                    add(
                        c,
                        &r,
                        &NewButton {
                            label: "Merge",
                            kind: ButtonKind::Merge,
                            command: "x",
                            workdir: "",
                            requires: &["T-9".into()]
                        },
                        "v"
                    )
                    .is_err(),
                    "unknown requirement"
                );
                assert!(
                    add(
                        c,
                        &r,
                        &NewButton {
                            label: "Merge",
                            kind: ButtonKind::Merge,
                            command: "echo safe\u{202E}lmth.exe",
                            workdir: "",
                            requires: &[]
                        },
                        "v"
                    )
                    .is_err(),
                    "bidi override"
                );
                assert!(
                    add(
                        c,
                        &r,
                        &NewButton {
                            label: "Merge",
                            kind: ButtonKind::Merge,
                            command: "echo a\u{200B}b",
                            workdir: "",
                            requires: &[]
                        },
                        "v"
                    )
                    .is_err(),
                    "zero-width"
                );
                let id = add(
                    c,
                    &r,
                    &NewButton {
                        label: "Merge",
                        kind: ButtonKind::Merge,
                        command: "echo merged $PWD; echo warn >&2",
                        workdir: &workdir,
                        requires: &["T-1".into()],
                    },
                    "v",
                )?;
                Ok((get(c, id)?.unwrap(), task))
            })
            .await
            .unwrap();

        // Not ready (agent, task) → can't claim; a wrong hash never can.
        let (id, hash) = (a.id, a.hash());
        let err = db.call(move |c| claim(c, id, &hash)).await.unwrap_err().to_string();
        assert!(err.contains("unlocks once T-1 is done"), "{err}");
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
            kind: ButtonKind::Tag,
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
