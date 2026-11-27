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
    /// AgentFlow's own command for this record ([`own_command`]), which runs in the server with
    /// its GitHub token. Found out when the button is claimed, never stored.
    #[serde(skip)]
    pub server: bool,
}

impl ActionRow {
    /// Short hash of exactly what the page showed: command and folder.
    pub fn hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.workdir.as_bytes());
        h.update([0]);
        h.update(self.command.as_bytes());
        crate::util::hex(&h.finalize()[..8])
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
        server: false,
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
    // The agent's own Merge button replaces the one AgentFlow put there by itself.
    if kind == ButtonKind::Merge {
        conn.execute(
            "UPDATE actions SET state = 'withdrawn' WHERE record_id = ?1 AND kind = 'merge' AND added_by = 'agentflow' AND state = 'waiting'",
            [record.id],
        )?;
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

/// What the confirmation before a press says. The owner, 2026-11-25 (D-263): "lets make these
/// dialogs more clear by saying that this will be a command thats executed. We can also make this
/// more specific in this case and say that pushing tags / merging PRs is always the same command
/// and the agent can just define the message themselves."
#[derive(Debug, Clone, Default)]
pub struct Confirm {
    /// "Push tag v0.4.1?", "Merge #48?", "Run \"Deploy\"?"
    pub title: String,
    /// What pressing it does, in words.
    pub lead: String,
    /// What the block under the lead is: "The command", "Message, written by lituus".
    pub block_label: String,
    /// The agent's part, shown whole: its command, or the message of AgentFlow's own command.
    pub block: String,
    /// The block is a command (monospace) rather than a message.
    pub block_is_command: bool,
    /// AgentFlow's own command, folded under the rest; empty when the block is the command.
    pub command: String,
}

/// The confirmation of a button. AgentFlow's own Merge and Push tag (the same command every time,
/// [`own_command`]) say what they do, read from the command itself; any other command is shown
/// whole as the command it is.
pub fn confirm(conn: &Connection, a: &ActionRow) -> anyhow::Result<Confirm> {
    use crate::prs::Described;
    let own = if own_command(conn, a)? { crate::prs::describe(&a.command) } else { None };
    Ok(match own {
        Some(Described::Merge { repo, number, method, head }) => {
            let how = match method.as_str() {
                "squash" => "squashed into one commit",
                "rebase" => "rebased",
                _ => "with a merge commit",
            };
            let pinned = if head.is_empty() {
                String::new()
            } else {
                format!(" GitHub refuses it if the branch got commits after {}.", &head[..7.min(head.len())])
            };
            Confirm {
                title: format!("Merge #{number}?"),
                lead: format!(
                    "This runs AgentFlow's merge command: #{number} in {repo} merges {how}, and its branch is \
                     deleted. It's the same command for every PR.{pinned}"
                ),
                command: a.command.clone(),
                ..Default::default()
            }
        }
        Some(Described::Tag { repo, tag, message }) => Confirm {
            title: format!("Push tag {tag}?"),
            lead: format!(
                "This runs AgentFlow's tag command: the newest commit on main in {repo} is tagged {tag}, and \
                 the tag is pushed. It's the same command for every release; only the message is the agent's."
            ),
            block_label: format!("Message, written by {}", a.added_by),
            block: message,
            command: a.command.clone(),
            ..Default::default()
        },
        None => {
            let dir = if a.workdir.is_empty() { "~" } else { &a.workdir };
            Confirm {
                title: format!("Run \"{}\"?", a.label),
                lead: format!(
                    "This runs a command that {} wrote, with bash in {dir} on the AgentFlow server. It runs \
                     exactly as shown.",
                    a.added_by
                ),
                block_label: "The command".into(),
                block: a.command.clone(),
                block_is_command: true,
                ..Default::default()
            }
        }
    })
}

/// Why the button is disabled, or Ok when it can be pressed.
pub fn enabled(conn: &Connection, a: &ActionRow) -> anyhow::Result<Result<(), String>> {
    match a.state.as_str() {
        "waiting" | "failed" => {}
        "running" => return Ok(Err("running now".into())),
        "done" => return Ok(Err("done".into())),
        s => return Ok(Err(s.into())),
    }
    // An open question that blocks the PR or release holds its buttons (Imaginarium's agent,
    // 2026-11-12: a Push tag button was live while the tag's name was still a question).
    let waiting: Option<i64> = conn
        .query_row(
            "SELECT q.number FROM links l JOIN records q ON q.id = l.from_id
             WHERE l.to_id = ?1 AND l.relation = 'blocks' AND q.kind = 'question' AND q.state = 'open'
             ORDER BY q.number LIMIT 1",
            [a.record_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(n) = waiting {
        return Ok(Err(format!("waits for the answer to Q-{n}")));
    }
    for req in &a.requires {
        let r = records::find_str(conn, req, None)?;
        if !records::is_finished(&r) {
            return Ok(Err(format!("unlocks once {} is {}", records::display_id(&r), records::finished_word(r.kind))));
        }
    }
    let record = records::get(conn, a.record_id)?;
    // A Merge button from before PRs were merged on GitHub (D-294) still runs only in order.
    if record.kind == Kind::Pr && a.is_merge() {
        if let Err(why) = crate::prs::merge_ready(&record) {
            return Ok(Err(why));
        }
        // A stacked PR merges after the one it's based on; one at the bottom must be based on
        // main.
        if let Some(o) =
            crate::prs::merge_order(conn, record.project_id)?.into_iter().find(|o| o.record.id == record.id)
        {
            if let Some(after) = o.after {
                return Ok(Err(format!("merge #{after} first")));
            }
            if let Err(why) = crate::prs::ready_in_order(&o) {
                return Ok(Err(why));
            }
        }
    }
    Ok(Ok(()))
}

/// Marks an action running, if it may be pressed and the page showed this exact command.
pub fn claim(conn: &Connection, id: i64, shown_hash: &str) -> anyhow::Result<ActionRow> {
    let mut a = get(conn, id)?.context("no such button")?;
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
    a.server = own_command(conn, &a)?;
    Ok(a)
}

/// Is this button's command the one AgentFlow makes itself for this very record: the merge of
/// this PR, or the tag of this release, in the project's own repository? Only those run with the
/// server's GitHub token. A command of the same shape that an agent wrote for another repository,
/// PR or tag, or put on a button of another kind (which skips the merge order), doesn't.
pub fn own_command(conn: &Connection, a: &ActionRow) -> anyhow::Result<bool> {
    use crate::prs::ServerCommand;
    if !a.workdir.is_empty() {
        return Ok(false);
    }
    let Some(command) = crate::prs::server_command(&a.command) else {
        return Ok(false);
    };
    let record = records::get(conn, a.record_id)?;
    let project = crate::projects::get_by_id(conn, record.project_id)?;
    let ours = |repo: &str| {
        crate::projects::looks_like_repo(project.repo.trim()) && repo.eq_ignore_ascii_case(project.repo.trim())
    };
    Ok(match (command, a.kind, record.kind) {
        (ServerCommand::Merge { repo, number }, ButtonKind::Merge, Kind::Pr) => ours(&repo) && number == record.number,
        (ServerCommand::Tag { repo, tag }, ButtonKind::Tag, Kind::Release) => ours(&repo) && tag == record.str("tag"),
        _ => false,
    })
}

/// A button's command as a job for the agents' user. It never carries the server's GitHub token:
/// the helper, its PATH and gh's settings there are the agents' to change.
fn hands_job(a: &ActionRow) -> crate::hands::Job {
    let dir: PathBuf = if a.workdir.is_empty() { crate::util::home_dir() } else { expand_home(&a.workdir) };
    crate::hands::Job::Exec {
        argv: vec!["bash".into(), "-c".into(), a.command.clone()],
        cwd: Some(dir),
        timeout_s: TIMEOUT.as_secs(),
    }
}

/// AgentFlow's own command, run by the server itself and as its user, with its GitHub token. The
/// command needs no project folder (GitHub's API does the work).
async fn run_here(command: &str, token: Vec<(String, String)>) -> anyhow::Result<crate::hands::ExecOut> {
    let run = tokio::process::Command::new("bash")
        .args(["-c", command])
        .current_dir(std::env::temp_dir())
        .envs(token)
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let Ok(out) = tokio::time::timeout(TIMEOUT, run).await else {
        return Ok(crate::hands::ExecOut { timed_out: true, ..Default::default() });
    };
    let out = out.context("couldn't start bash")?;
    Ok(crate::hands::ExecOut {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        timed_out: false,
    })
}

pub struct RunResult {
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub output: String,
}

/// Runs the command with bash and a 10-minute limit. An agent's own command runs in its folder
/// as the agents' user (see [`crate::hands`]). AgentFlow's own Merge and Push tag commands run in
/// the server when it has a GitHub token of its own, so the token never reaches the agents' user;
/// without one they use the agents' `gh` login like any other button. Never called by an agent.
pub async fn execute(a: &ActionRow) -> RunResult {
    let dir: PathBuf = if a.workdir.is_empty() { crate::util::home_dir() } else { expand_home(&a.workdir) };
    let token = if a.server { crate::secrets::gh_env() } else { Vec::new() };
    let out = if token.is_empty() {
        crate::hands::run_async::<crate::hands::ExecOut>(hands_job(a)).await
    } else {
        run_here(&a.command, token).await
    };
    match out {
        Ok(out) if out.timed_out => {
            RunResult { ok: false, exit_code: None, output: "Stopped after 10 minutes.".into() }
        }
        Err(e) => {
            RunResult { ok: false, exit_code: None, output: format!("Couldn't start it in {}: {e:#}", dir.display()) }
        }
        Ok(out) => {
            let mut text = out.stdout.clone();
            if !out.stderr.trim().is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&out.stderr);
            }
            if text.len() > MAX_OUTPUT {
                let cut = text.char_indices().map(|(i, _)| i).take_while(|&i| i <= MAX_OUTPUT).last().unwrap_or(0);
                text.truncate(cut);
                text.push_str("\n[output cut at 64 KB]");
            }
            RunResult { ok: out.success(), exit_code: out.code, output: crate::activity::redact(&text) }
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
    if r.ok && record.kind == Kind::Release && a.kind == ButtonKind::Tag {
        mark_tagged(conn, &record, "owner")?;
    }
    Ok(())
}

/// A release whose tag is pushed is tagged, and the milestone it closes is closed with that tag
/// (Imaginarium's agent, 2026-11-12). Its tag buttons still waiting are withdrawn: pressed, they
/// would only collide with the tag (the consultant, 2026-11-19, Imaginarium's R-1).
pub fn mark_tagged(conn: &Connection, record: &Record, by: &str) -> anyhow::Result<()> {
    records::update(
        conn,
        record,
        by,
        records::Patch { state: Some("tagged".into()), quiet: true, ..Default::default() },
    )?;
    if let Some(mid) = record.milestone_id {
        let m = crate::projects::get_milestone(conn, mid)?;
        if m.state != "closed" {
            let patch = crate::projects::MilestonePatch {
                state: Some("closed".into()),
                tag: Some(record.str("tag").to_owned()),
                ..Default::default()
            };
            crate::projects::update_milestone(conn, &m, &patch, by)?;
        }
    }
    conn.execute(
        "UPDATE actions SET state = 'withdrawn' WHERE record_id = ?1 AND kind = 'tag' AND state IN ('waiting', 'failed')",
        [record.id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::github::fake::pr;

    #[test]
    fn a_pushed_tag_closes_its_milestone() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let p = crate::projects::create(c, "v", "V", "t")?;
            let m = crate::projects::add_milestone(
                c,
                p.id,
                &crate::projects::NewMilestone {
                    name: "M4",
                    title: "Keybinds",
                    description: "",
                    parent: None,
                    before: None,
                },
                "v",
            )?;
            let mut data = serde_json::Map::new();
            data.insert("tag".into(), "v0.0.5-m4".into());
            let rel = records::create(
                c,
                p.id,
                "v",
                records::NewRecord {
                    kind: Some(Kind::Release),
                    title: "M4".into(),
                    state: Some("waiting".into()),
                    milestone_id: Some(m.id),
                    data,
                    ..Default::default()
                },
            )?;
            let b = NewButton {
                label: "Push tag v0.0.5-m4",
                kind: ButtonKind::Tag,
                command: "git push origin v0.0.5-m4",
                workdir: "",
                requires: &[],
            };
            let id = add(c, &rel, &b, "v")?;
            let row = get(c, id)?.unwrap();
            finish(c, &row, &RunResult { ok: true, exit_code: Some(0), output: String::new() })?;
            let m = crate::projects::get_milestone(c, m.id)?;
            assert_eq!((m.state.as_str(), m.tag.as_str()), ("closed", "v0.0.5-m4"));
            assert_eq!(records::get(c, rel.id)?.state, "tagged");
            Ok(())
        })
        .unwrap();
    }

    /// A project with repository o/r and its PR #48, with a Merge button of AgentFlow's own as
    /// it made them before PRs were merged on GitHub (D-294).
    fn project_with_pr(c: &Connection) -> anyhow::Result<(Record, ActionRow)> {
        let p = crate::projects::create(c, "v", "V", "t")?;
        let patch = crate::projects::ProjectPatch { repo: Some("o/r".into()), ..Default::default() };
        crate::projects::update(c, p.id, &patch, "t")?;
        let p = crate::projects::get_by_id(c, p.id)?;
        let (r, _) = crate::prs::sync(c, &p, "o/r", &pr(48, "main", "b48"), "poller")?;
        let command = crate::prs::merge_command("o/r", 48, "merge");
        let b = NewButton { label: "Merge", kind: ButtonKind::Merge, command: &command, workdir: "", requires: &[] };
        let own = get(c, add(c, &r, &b, "agentflow")?)?.context("the PR's own Merge button")?;
        Ok((r, own))
    }

    #[test]
    fn the_servers_token_stays_out_of_hands() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let (_, own) = project_with_pr(c)?;
            assert!(own_command(c, &own)?);
            // What goes over the socket to the agents' user has no place for a token.
            let job = serde_json::to_string(&hands_job(&own))?;
            assert!(!job.contains("\"env\"") && !job.contains("GH_TOKEN"), "{job}");
            Ok(())
        })
        .unwrap();
    }

    #[tokio::test]
    async fn own_commands_run_in_the_server_with_the_token() {
        let token = vec![("GH_TOKEN".to_owned(), "t0k".to_owned())];
        let out = run_here("echo $GH_TOKEN in $PWD; echo warn >&2; exit 4", token).await.unwrap();
        assert_eq!((out.code, out.stderr.as_str()), (Some(4), "warn\n"));
        assert!(out.stdout.starts_with("t0k in "), "{}", out.stdout);
    }

    #[test]
    fn only_agentflows_own_commands_get_the_token() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let (r, own) = project_with_pr(c)?;
            assert_eq!(own.command, crate::prs::merge_command("o/r", 48, "merge"));
            assert!(own_command(c, &own)?, "AgentFlow's Merge button for this PR");
            // An agent's button with the same shape, for another repository or another PR.
            let agents = |kind, command: &str| -> anyhow::Result<ActionRow> {
                let b = NewButton { label: "Merge", kind, command, workdir: "", requires: &[] };
                Ok(get(c, add(c, &r, &b, "v")?)?.unwrap())
            };
            // Only a claimed button knows it: the page's rows never say "server".
            let (id, hash) = (own.id, own.hash());
            assert!(!own.server && claim(c, id, &hash)?.server);
            for (kind, command) in [
                (ButtonKind::Merge, crate::prs::merge_command("o/other", 7, "merge")),
                (ButtonKind::Merge, crate::prs::merge_command("o/r", 7, "merge")),
                (ButtonKind::Other, crate::prs::merge_command("o/r", 48, "merge")),
                (ButtonKind::Other, crate::prs::tag_command("o/r", "v1", "v1")),
            ] {
                let a = agents(kind, &command)?;
                assert!(crate::prs::is_server_command(&a.command));
                assert!(!own_command(c, &a)?, "{kind:?} {command}");
            }
            Ok(())
        })
        .unwrap();
    }

    /// D-263: the confirmation says a command runs; AgentFlow's own say what they do, in words read
    /// from the command, and anything else is shown whole as the command.
    #[test]
    fn the_confirmation_says_what_runs() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let (r, own) = project_with_pr(c)?;
            let m = confirm(c, &own)?;
            assert_eq!(m.title, "Merge #48?");
            assert!(m.lead.contains("#48 in o/r merges with a merge commit"), "{}", m.lead);
            assert!(m.lead.contains("the same command for every PR"), "{}", m.lead);
            assert!(m.block.is_empty() && m.command == own.command, "the command one click away");

            // A release's Push tag: the agent's message, the command folded.
            let mut data = serde_json::Map::new();
            data.insert("tag".into(), "v0.4.1".into());
            let rel = records::create(
                c,
                r.project_id,
                "v",
                records::NewRecord {
                    kind: Some(Kind::Release),
                    title: "Lituus 0.4.1".into(),
                    state: Some("waiting".into()),
                    data,
                    ..Default::default()
                },
            )?;
            let message = "Lituus 0.4.1: fixes\n\n- Every dropdown shows what's applied.";
            let command = crate::prs::tag_command("o/r", "v0.4.1", message);
            let tag = NewButton {
                label: "Push tag v0.4.1",
                kind: ButtonKind::Tag,
                command: &command,
                workdir: "",
                requires: &[],
            };
            let tag = get(c, add(c, &rel, &tag, "v")?)?.unwrap();
            let t = confirm(c, &tag)?;
            assert_eq!(t.title, "Push tag v0.4.1?");
            assert!(
                t.lead.contains("main in o/r is tagged v0.4.1") && t.lead.contains("only the message is the agent")
            );
            assert_eq!((t.block_label.as_str(), t.block.as_str()), ("Message, written by v", message));
            assert!(!t.block_is_command && t.command == command);

            // An agent's own command, and AgentFlow's command shape for another repository: whole.
            let theirs = NewButton {
                label: "Deploy docs",
                kind: ButtonKind::Other,
                command: "make docs && rsync -a site/ host:",
                workdir: "~/vinculum",
                requires: &[],
            };
            let other = crate::prs::merge_command("o/other", 48, "merge");
            let elsewhere =
                NewButton { label: "Merge", kind: ButtonKind::Merge, command: &other, workdir: "", requires: &[] };
            for (b, title) in [(theirs, "Run \"Deploy docs\"?"), (elsewhere, "Run \"Merge\"?")] {
                let a = get(c, add(c, &r, &b, "v")?)?.unwrap();
                let x = confirm(c, &a)?;
                assert_eq!(x.title, title);
                assert!(x.block_is_command && x.block == a.command && x.command.is_empty());
                assert!(x.lead.contains("a command that v wrote, with bash in"), "{}", x.lead);
            }
            Ok(())
        })
        .unwrap();
    }

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
            server: false,
        };
        let r = execute(&a).await;
        assert!(!r.ok);
        assert_eq!(r.exit_code, Some(3));
        assert!(r.output.contains("[redacted]") && !r.output.contains("af_prj_"));
    }
}
