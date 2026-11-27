//! Pull requests: kept in step with GitHub by a poller, put in merge order from their bases, and
//! marked ready by their project's agent.

use std::collections::HashMap;

use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::github::PrInfo;
use crate::projects::Project;
use crate::records::{self, Kind, NewRecord, Patch, Query, Record};

/// The name GitHub's changes are recorded under, in revisions and events alike.
pub const GITHUB: &str = "github";

/// What changed when a PR was synced; the poller turns these into events and notifications.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncResult {
    pub created: bool,
    pub merged: bool,
    pub closed: bool,
    pub ci_failed: bool,
    /// GitHub found conflicts with its base: the agent merges the base in (Q-37).
    pub conflicted: bool,
    pub became_mergeable: bool,
}

/// Creates or updates the PR record for `info`. Fields the agent sets (ready, notes, links) stay.
pub fn sync(
    conn: &Connection,
    project: &Project,
    repo: &str,
    info: &PrInfo,
    author: &str,
) -> anyhow::Result<(Record, SyncResult)> {
    let state = match info.state.as_str() {
        "merged" => "merged",
        "closed" => "closed",
        _ => "open",
    };
    let mut data = Map::new();
    data.insert("repo".into(), repo.into());
    data.insert("url".into(), info.url.clone().into());
    data.insert("base".into(), info.base.clone().into());
    data.insert("head".into(), info.head.clone().into());
    if !info.head_sha.is_empty() {
        data.insert("head_sha".into(), info.head_sha.clone().into());
    }
    data.insert("draft".into(), info.draft.into());
    data.insert("mergeable".into(), info.mergeable.clone().into());
    data.insert("ci".into(), info.ci.clone().into());

    let existing = records::try_find(
        conn,
        &records::RecordRef { project: Some(project.slug.clone()), kind: Kind::Pr, number: info.number },
        None,
    )?;
    let Some(old) = existing else {
        // Seen for the first time: nothing to announce yet.
        let r = records::create(
            conn,
            project.id,
            author,
            NewRecord {
                kind: Some(Kind::Pr),
                number: Some(info.number),
                title: info.title.clone(),
                state: Some(state.into()),
                data,
                ..Default::default()
            },
        )?;
        link_by_branch(conn, project.id, &r, &info.head, author)?;
        return Ok((r, SyncResult { created: true, merged: state == "merged", ..Default::default() }));
    };
    let was_ready = merge_ready(&old).is_ok();
    let mut result = SyncResult {
        merged: state == "merged" && old.state != "merged",
        closed: state == "closed" && old.state != "closed",
        ci_failed: info.ci == "failing" && old.str("ci") != "failing",
        conflicted: state == "open" && info.mergeable == "conflicting" && old.str("mergeable") != "conflicting",
        ..Default::default()
    };
    // The revision keeps what changed; the events below say what matters (merged, closed, CI).
    let quiet = true;
    let (r, _) = records::update(
        conn,
        &old,
        author,
        Patch { title: Some(info.title.clone()), state: Some(state.into()), data, quiet, ..Default::default() },
    )?;
    result.became_mergeable = !was_ready && merge_ready(&r).is_ok();
    link_by_branch(conn, project.id, &r, &info.head, author)?;
    withdraw_merge_buttons(conn, &r)?;
    // The agent hears about what GitHub did to its PR, and fixes a conflict before the owner
    // merges on GitHub (Q-37).
    for (happened, kind, what) in [
        (result.merged, "pr.merged", format!("#{} merged: {}", r.number, r.title)),
        (result.closed, "pr.closed", format!("#{} closed without merging", r.number)),
        (result.ci_failed, "pr.ci_failed", format!("CI failing on #{}", r.number)),
        (
            result.conflicted,
            "pr.conflicting",
            format!("#{} has conflicts with {}: merge {} into it", r.number, r.str("base"), r.str("base")),
        ),
    ] {
        if happened {
            records::event(conn, Some(project.id), Some(r.id), kind, &what, author)?;
        }
    }
    Ok((r, result))
}

/// A repository's main line: where a PR that isn't stacked on another open PR must point.
pub fn is_main_branch(base: &str) -> bool {
    matches!(base, "main" | "master")
}

/// Whether a PR can be merged now, in its place in the merge order. A PR at the bottom of the
/// order must be based on main: one based on a side branch with no open PR would otherwise merge
/// into that branch (Imaginarium's agent, 2026-11-12: "a silent wrong merge, from one press").
pub fn ready_in_order(o: &Ordered) -> Result<(), String> {
    merge_ready(&o.record)?;
    if o.after.is_none() && !o.base_ok {
        return Err(format!("based on {}, which isn't main and has no open PR", o.record.str("base")));
    }
    Ok(())
}

/// The line that ends a release's message in its Push tag command. A message never contains it,
/// so nothing in the message can end the heredoc early.
const MESSAGE_END: &str = "AGENTFLOW_RELEASE_MESSAGE";

/// A release's tag message (the owner, 2026-11-24, D-254: the title and notes in the command
/// itself, shown whole in the confirmation): the title, a blank line, the notes. git shows it as
/// the annotated tag's message, and the release workflow turns it into the GitHub release's notes.
pub fn tag_message(title: &str, notes: &str) -> String {
    let notes = notes.trim();
    if notes.is_empty() { title.trim().to_owned() } else { format!("{}\n\n{notes}", title.trim()) }
}

/// The message fits the command the owner reads whole (40 lines, 2,000 characters in all).
pub fn check_tag_message(message: &str) -> anyhow::Result<()> {
    if message.lines().any(|l| l.trim() == MESSAGE_END) {
        anyhow::bail!("the notes can't have a line {MESSAGE_END}");
    }
    if message.lines().count() > 32 || message.chars().count() > 1500 {
        anyhow::bail!(
            "the title and notes go into the Push tag command, which the owner reads whole: at most 32 lines and \
             1,500 characters (a first paragraph, then a short list)"
        );
    }
    if message.contains("\n\n\n") || message.lines().any(|l| l.contains(&" ".repeat(24)) || l.contains("\t\t\t")) {
        anyhow::bail!(
            "the notes: no runs of blank lines or long runs of spaces; the confirmation shows them as written"
        );
    }
    if message.chars().any(|c| c.is_control() && c != '\n') {
        anyhow::bail!("the notes: plain text, no control characters");
    }
    Ok(())
}

/// A release's tag through GitHub's API (the owner, 2026-11-13: the server needs no project
/// folder): an annotated tag `tag` on main's newest commit, its message the release's title and
/// notes (D-254). Tag and repository are checked values (letters, digits and `._-+`; owner/name);
/// the message goes in a quoted heredoc, so the shell takes it as it is.
pub fn tag_command(repo: &str, tag: &str, message: &str) -> String {
    format!(
        "sha=$(gh api repos/{repo}/git/ref/heads/main --jq .object.sha) && \\\n\
         msg=$(cat <<'{MESSAGE_END}'\n{message}\n{MESSAGE_END}\n) && \\\n\
         obj=$(gh api -X POST repos/{repo}/git/tags -f tag={tag} -f message=\"$msg\" -f object=\"$sha\" -f type=commit --jq .sha) && \\\n\
         gh api -X POST repos/{repo}/git/refs -f ref=refs/tags/{tag} -f sha=\"$obj\" --jq .ref"
    )
}

/// The command of releases planned before D-254: the tag's name was its message.
fn tag_command_bare(repo: &str, tag: &str) -> String {
    format!(
        "sha=$(gh api repos/{repo}/git/ref/heads/main --jq .object.sha) && \\\n\
         obj=$(gh api -X POST repos/{repo}/git/tags -f tag={tag} -f message={tag} -f object=\"$sha\" -f type=commit --jq .sha) && \\\n\
         gh api -X POST repos/{repo}/git/refs -f ref=refs/tags/{tag} -f sha=\"$obj\" --jq .ref"
    )
}

/// The command a PR's Merge button runs: the project's merge method, deleting the branch so
/// GitHub moves the next PR of a stack onto main.
pub fn merge_command(repo: &str, number: i64, method: &str) -> String {
    let method = if ["merge", "squash", "rebase"].contains(&method) { method } else { "merge" };
    format!("gh pr merge {number} -R {repo} --{method} --delete-branch")
}

/// The same, merging only if the PR's branch is still at the commit `head`: the one AgentFlow
/// knew when the owner looked. A push after that makes gh refuse, instead of merging commits
/// nobody saw. Without a known commit it is the plain command.
pub fn merge_command_at(repo: &str, number: i64, method: &str, head: &str) -> String {
    let plain = merge_command(repo, number, method);
    if is_commit(head) { format!("{plain} --match-head-commit {head}") } else { plain }
}

fn is_commit(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64) && sha.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// What a command AgentFlow makes itself does: merges a PR, or tags a repository's main.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerCommand {
    Merge { repo: String, number: i64 },
    Tag { repo: String, tag: String },
}

/// Is this exactly a command AgentFlow makes itself (a Merge or Push tag button's)? That is only
/// its shape: whether it runs with the server's GitHub token also depends on the button's record
/// (see [`crate::actions::own_command`]).
pub fn is_server_command(cmd: &str) -> bool {
    server_command(cmd).is_some()
}

/// The command taken apart, when it is exactly one AgentFlow makes itself.
pub fn server_command(cmd: &str) -> Option<ServerCommand> {
    let safe = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-+/".contains(c));
    let w: Vec<&str> = cmd.split(' ').collect();
    if let ["gh", "pr", "merge", n, "-R", repo, method, "--delete-branch", pin @ ..] = w.as_slice() {
        let m = method.trim_start_matches("--");
        let number = n.parse::<i64>().ok()?;
        let head = match pin {
            [] => "",
            ["--match-head-commit", sha] if is_commit(sha) => *sha,
            _ => return None,
        };
        let ok = crate::projects::looks_like_repo(repo)
            && safe(repo)
            && cmd == merge_command_at(repo, number, m, head)
            && ["merge", "squash", "rebase"].contains(&m);
        return ok.then(|| ServerCommand::Merge { repo: (*repo).to_owned(), number });
    }
    let between = |a: &str, b: &str| cmd.split_once(a).and_then(|(_, r)| r.split_once(b)).map(|(x, _)| x.to_owned());
    // The tag from the last line, after the message.
    let tag = cmd.rsplit_once(" -f ref=refs/tags/").and_then(|(_, r)| r.split_once(' ')).map(|(t, _)| t.to_owned());
    let (Some(repo), Some(tag)) = (between("gh api repos/", "/git/ref/heads/main"), tag) else {
        return None;
    };
    if !(crate::projects::looks_like_repo(&repo) && safe(&repo) && safe(&tag)) {
        return None;
    }
    // The message is whatever stands between the heredoc's lines; the rest must be exactly ours.
    let start = format!("<<'{MESSAGE_END}'\n");
    let end = format!("\n{MESSAGE_END}\n)");
    let ours = cmd == tag_command_bare(&repo, &tag)
        || cmd
            .split_once(&start)
            .and_then(|(_, r)| r.split_once(&end))
            .is_some_and(|(message, _)| check_tag_message(message).is_ok() && cmd == tag_command(&repo, &tag, message));
    ours.then_some(ServerCommand::Tag { repo, tag })
}

/// An own command's details in the words of its confirmation (D-263): a merge's method and the
/// commit it is pinned to, a tag's message. Only for a command [`server_command`] takes apart, so
/// what the dialog says is read from what runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Described {
    Merge { repo: String, number: i64, method: String, head: String },
    Tag { repo: String, tag: String, message: String },
}

pub fn describe(cmd: &str) -> Option<Described> {
    Some(match server_command(cmd)? {
        ServerCommand::Merge { repo, number } => {
            let w: Vec<&str> = cmd.split(' ').collect();
            let method = w.get(6)?.trim_start_matches("--").to_owned();
            let head = w.get(9).map(|s| (*s).to_owned()).unwrap_or_default();
            Described::Merge { repo, number, method, head }
        }
        ServerCommand::Tag { repo, tag } => {
            let message = if cmd == tag_command_bare(&repo, &tag) {
                tag.clone()
            } else {
                let start = format!("<<'{MESSAGE_END}'\n");
                let end = format!("\n{MESSAGE_END}\n)");
                cmd.split_once(&start)?.1.split_once(&end)?.0.to_owned()
            };
            Described::Tag { repo, tag, message }
        }
    })
}

/// PRs are merged on GitHub (the owner, 2026-11-26, Q-37: "maybe we can delegate this to github?
/// like not even have the merge functionality inside agentflow?"): a Merge button from before
/// is withdrawn when its PR syncs.
fn withdraw_merge_buttons(conn: &Connection, pr: &Record) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE actions SET state = 'withdrawn' WHERE record_id = ?1 AND kind = 'merge' AND state IN ('waiting', 'failed')",
        [pr.id],
    )?;
    Ok(())
}

/// A PR carries out the tasks whose branch is its head branch (Imaginarium's agent, 2026-11-12:
/// then update_pr with implements is only needed when the branch doesn't match).
fn link_by_branch(conn: &Connection, project_id: i64, pr: &Record, head: &str, author: &str) -> anyhow::Result<()> {
    if head.is_empty() {
        return Ok(());
    }
    let mut stmt = conn.prepare(
        "SELECT id FROM records WHERE project_id = ?1 AND kind = 'task' AND state != 'archived' AND json_extract(data, '$.branch') = ?2
         AND id NOT IN (SELECT to_id FROM links WHERE from_id = ?3 AND relation = 'implements')",
    )?;
    let ids: Vec<i64> =
        stmt.query_map(rusqlite::params![project_id, head, pr.id], |r| r.get(0))?.collect::<Result<_, _>>()?;
    for id in ids {
        let t = records::get(conn, id)?;
        records::link(conn, pr, &t, "implements", author)?;
    }
    Ok(())
}

/// Whether a PR can be merged now, apart from its place in the merge order; why not otherwise.
pub fn merge_ready(pr: &Record) -> Result<(), String> {
    if pr.state != "open" {
        return Err(format!("already {}", pr.state));
    }
    // Problems on GitHub first: they say more than "not marked ready".
    if pr.data.get("draft").and_then(Value::as_bool).unwrap_or(false) {
        return Err("still a draft".into());
    }
    if pr.str("ci") == "failing" {
        return Err("CI failing".into());
    }
    if pr.str("mergeable") == "conflicting" {
        return Err("has conflicts".into());
    }
    if pr.str("ci") == "pending" {
        return Err("CI still running".into());
    }
    Ok(())
}

/// One PR in the project's merge order.
#[derive(Debug, Clone)]
pub struct Ordered {
    pub record: Record,
    /// 1-based place in the order.
    pub position: usize,
    /// The open PR this one is based on, which must be merged first.
    pub after: Option<i64>,
    /// At the bottom of the order: whether it points at main (or at the branch of a PR just merged,
    /// which GitHub moves onto main).
    pub base_ok: bool,
}

/// Open PRs in merge order: a PR based on another PR's branch comes after it. Independent stacks
/// follow each other by their lowest number.
pub fn merge_order(conn: &Connection, project_id: i64) -> anyhow::Result<Vec<Ordered>> {
    let mut open = records::query(
        conn,
        &Query {
            project_id: Some(project_id),
            kinds: vec![Kind::Pr],
            states: vec!["open".into()],
            limit: 0,
            ..Default::default()
        },
    )?;
    open.sort_by_key(|r| r.number);
    let by_head: HashMap<String, i64> = open.iter().map(|r| (r.str("head").to_owned(), r.number)).collect();
    let merged_heads: std::collections::HashSet<String> = records::query(
        conn,
        &Query {
            project_id: Some(project_id),
            kinds: vec![Kind::Pr],
            states: vec!["merged".into()],
            limit: 0,
            ..Default::default()
        },
    )?
    .iter()
    .map(|r| r.str("head").to_owned())
    .collect();
    let base_ok = |r: &Record| {
        let b = r.str("base");
        b.is_empty() || is_main_branch(b) || merged_heads.contains(b)
    };
    let parent = |r: &Record| by_head.get(r.str("base")).copied().filter(|&n| n != r.number);
    let mut out = Vec::new();
    let mut placed = std::collections::HashSet::new();
    // Walk each stack from its root (a PR whose base isn't another open PR).
    fn place(
        r: &Record,
        open: &[Record],
        parent: &dyn Fn(&Record) -> Option<i64>,
        placed: &mut std::collections::HashSet<i64>,
        out: &mut Vec<Ordered>,
    ) {
        if !placed.insert(r.number) {
            return;
        }
        out.push(Ordered { record: r.clone(), position: out.len() + 1, after: parent(r), base_ok: false });
        for child in open.iter().filter(|c| parent(c) == Some(r.number)) {
            place(child, open, parent, placed, out);
        }
    }
    for r in open.iter().filter(|r| parent(r).is_none()) {
        place(r, &open, &parent, &mut placed, &mut out);
    }
    // A cycle of bases can't be ordered; list what's left at the end rather than hide it.
    for r in &open {
        if !placed.contains(&r.number) {
            placed.insert(r.number);
            out.push(Ordered { record: r.clone(), position: out.len() + 1, after: parent(r), base_ok: false });
        }
    }
    for o in &mut out {
        o.base_ok = base_ok(&o.record);
    }
    Ok(out)
}

/// A repository's tags as GitHub has them now, in place of those kept before.
pub fn set_repo_tags(c: &Connection, project_id: i64, tags: &[crate::github::RepoTag]) -> anyhow::Result<()> {
    let tx = c.unchecked_transaction()?;
    tx.execute("DELETE FROM repo_tags WHERE project_id = ?1", [project_id])?;
    for t in tags {
        tx.execute(
            "INSERT OR IGNORE INTO repo_tags (project_id, name, at, title) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![project_id, t.name, t.at, t.title],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// The project repository's newest tags (at most `limit`), and how many it has in all. A tag
/// without a message of its own takes the title of the release or milestone it closed.
pub fn repo_tags(c: &Connection, project_id: i64, limit: i64) -> anyhow::Result<(Vec<crate::github::RepoTag>, i64)> {
    let total: i64 = c.query_row("SELECT count(*) FROM repo_tags WHERE project_id = ?1", [project_id], |r| r.get(0))?;
    let mut stmt = c.prepare(
        "SELECT t.name, t.at, CASE WHEN t.title != '' AND t.title != t.name THEN t.title ELSE coalesce(
             (SELECT r.title FROM records r WHERE r.project_id = t.project_id AND r.kind = 'release'
               AND json_extract(r.data, '$.tag') = t.name ORDER BY r.id DESC LIMIT 1),
             (SELECT m.title FROM milestones m WHERE m.project_id = t.project_id AND m.tag = t.name LIMIT 1), '') END
         FROM repo_tags t WHERE t.project_id = ?1 ORDER BY t.at DESC, t.name DESC LIMIT ?2",
    )?;
    let tags = stmt
        .query_map(rusqlite::params![project_id, limit], |r| {
            Ok(crate::github::RepoTag { name: r.get(0)?, at: r.get(1)?, title: r.get(2)? })
        })?
        .collect::<Result<_, _>>()?;
    Ok((tags, total))
}

/// Every PR the release waits for is merged.
pub fn release_ready(c: &Connection, r: &Record) -> anyhow::Result<bool> {
    let waits: Vec<String> = r
        .data
        .get("waits_for")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    for w in waits {
        if records::find_str(c, &w, None)?.state != "merged" {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Brings a project's older PRs (from before AgentFlow) in as PR records, so tasks can link to
/// them: those of `wanted` that aren't records yet. Returns the PRs added and how many task links
/// the import made by branch. The agent hears of it in one line, not one per PR.
pub fn import(
    c: &mut rusqlite::Connection,
    p: &crate::projects::Project,
    repo: &str,
    wanted: &[crate::github::PrInfo],
    by: &str,
) -> anyhow::Result<(Vec<i64>, i64)> {
    let tx = c.transaction()?;
    // The agent hears of the import in one line, not one per PR: if it had read all its
    // news before, the import's own events are marked read too.
    let caught_up = crate::waiting::unread_count(&tx, p.id, &p.slug)? == 0;
    let mut added = Vec::new();
    for info in wanted {
        let exists = crate::records::try_find(
            &tx,
            &crate::records::RecordRef {
                project: Some(p.slug.clone()),
                kind: crate::records::Kind::Pr,
                number: info.number,
            },
            None,
        )?
        .is_some();
        if !exists {
            sync(&tx, p, repo, info, "import")?;
            added.push(info.number);
        }
    }
    if caught_up {
        let latest: i64 = tx.query_row("SELECT coalesce(max(seq), 0) FROM events", [], |r| r.get(0))?;
        crate::waiting::set_cursor(&tx, p.id, latest)?;
    }
    let linked: i64 = tx.query_row(
        "SELECT count(*) FROM links l JOIN records r ON r.id = l.from_id
         WHERE r.project_id = ?1 AND r.kind = 'pr' AND l.relation = 'implements' AND l.author = 'import'",
        [p.id],
        |r| r.get(0),
    )?;
    if !added.is_empty() {
        crate::records::event(
            &tx,
            Some(p.id),
            None,
            "pr.imported",
            &format!(
                "{} older PRs imported as records ({}–{}); link tasks to them with link (implements)",
                added.len(),
                added.iter().min().unwrap_or(&0),
                added.iter().max().unwrap_or(&0)
            ),
            by,
        )?;
    }
    tx.commit()?;
    Ok((added, linked))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn only_agentflows_own_commands_get_the_token() {
        assert!(is_server_command(&merge_command("giosuel/agent-flow", 79, "merge")));
        assert!(is_server_command(&merge_command("giosuel/agent-flow", 3, "squash")));
        assert!(is_server_command(&tag_command("giosuel/agent-flow", "v0.0.11", "v0.0.11")));
        assert!(is_server_command(&tag_command_bare("giosuel/agent-flow", "v0.0.11")), "releases planned before D-254");
        // The title and notes (D-254): taken as written, in a quoted heredoc.
        let msg =
            tag_message("AgentFlow 0.1.0", "The first release.\n\n- Questions $(id) and `whoami`\n- -f tag=x ' \" ;");
        assert!(check_tag_message(&msg).is_ok(), "{msg}");
        let cmd = tag_command("giosuel/agent-flow", "v0.1.0", &msg);
        assert!(is_server_command(&cmd), "{cmd}");
        assert!(
            cmd.lines().count() <= 40
                && cmd.contains("msg=$(cat <<'AGENTFLOW_RELEASE_MESSAGE'\nAgentFlow 0.1.0\n\nThe first")
        );
        assert!(!is_server_command(
            &cmd.replace("\nAGENTFLOW_RELEASE_MESSAGE\n)", "\nAGENTFLOW_RELEASE_MESSAGE\n);id")
        ));
        let sneaky = tag_command(
            "giosuel/agent-flow",
            "v1",
            "x\nAGENTFLOW_RELEASE_MESSAGE\n) && id && msg=$(cat <<'AGENTFLOW_RELEASE_MESSAGE'\ny",
        );
        assert!(!is_server_command(&sneaky), "a message can't end the heredoc");
        assert!(check_tag_message("x\nAGENTFLOW_RELEASE_MESSAGE").is_err());
        assert!(check_tag_message(&"a line\n".repeat(40)).is_err(), "the owner reads all of it");
        assert_eq!(tag_message("  Title ", "  "), "Title");
        // Run for real by bash, with a gh that writes down what it was given: the message arrives as written.
        let dir = tempfile::tempdir().unwrap();
        let gh = dir.path().join("gh");
        let log = dir.path().join("log");
        std::fs::write(
            &gh,
            format!(
                "#!/bin/bash\nfor a in \"$@\"; do case \"$a\" in message=*) printf '%s' \"${{a#message=}}\" > {}; esac; done\necho abc123\n",
                log.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(&cmd)
            .env("PATH", format!("{}:/usr/bin:/bin", dir.path().display()))
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(std::fs::read_to_string(&log).unwrap(), msg, "the tag's message is the title and notes, untouched");
        assert!(!is_server_command("gh pr merge 79 -R giosuel/agent-flow --merge --delete-branch; env"));
        assert!(!is_server_command(&format!("{}; curl x", tag_command("giosuel/agent-flow", "v1", "v1"))));
        assert!(!is_server_command(&tag_command("giosuel/agent-flow", "v1;id", "v1")));
        assert!(!is_server_command("cargo build"));
    }
    use crate::github::fake::pr;

    #[test]
    fn a_pr_on_a_side_branch_isnt_mergeable() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let p = crate::projects::create(c, "v", "V", "t")?;
            sync(c, &p, "o/r", &pr(60, "some-branch", "b60"), "poller")?;
            sync(c, &p, "o/r", &pr(61, "main", "b61"), "poller")?;
            sync(c, &p, "o/r", &pr(62, "b61", "b62"), "poller")?;
            let order = merge_order(c, p.id)?;
            let of = |n: i64| order.iter().find(|o| o.record.number == n).unwrap();
            assert_eq!(
                ready_in_order(of(60)).unwrap_err(),
                "based on some-branch, which isn't main and has no open PR"
            );
            assert!(ready_in_order(of(61)).is_ok());
            assert!(ready_in_order(of(62)).is_ok(), "stacked on an open PR: its turn comes after #61");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_merge_command_pins_its_commit() {
        let first = "a".repeat(40);
        let pinned = merge_command_at("o/r", 48, "merge", &first);
        assert_eq!(pinned, format!("gh pr merge 48 -R o/r --merge --delete-branch --match-head-commit {first}"));
        assert!(is_server_command(&pinned));
        assert_eq!(merge_command_at("o/r", 48, "squash", ""), merge_command("o/r", 48, "squash"), "no commit known");
        // Nothing else may follow the command.
        for bad in [
            "--match-head-commit abc",
            "--match-head-commit $(id)",
            "--admin",
            &format!("--match-head-commit {first} --admin"),
        ] {
            assert!(!is_server_command(&format!("{} {bad}", merge_command("o/r", 48, "merge"))), "{bad}");
        }
    }

    #[test]
    fn prs_are_merged_on_github_and_conflicts_reach_the_agent() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let p = crate::projects::create(c, "v", "V", "t")?;
            let patch = crate::projects::ProjectPatch { repo: Some("o/r".into()), ..Default::default() };
            crate::projects::update(c, p.id, &patch, "owner")?;
            let p = crate::projects::get_by_id(c, p.id)?;
            let (r, _) = sync(c, &p, "o/r", &pr(48, "main", "b48"), "poller")?;
            assert!(crate::actions::for_record(c, r.id)?.is_empty(), "no Merge button (Q-37)");
            // One from before is withdrawn when the PR syncs.
            let old = crate::actions::NewButton {
                label: "Merge",
                kind: crate::actions::ButtonKind::parse("merge")?,
                command: "gh pr merge 48 -R o/r --merge --delete-branch",
                workdir: "",
                requires: &[],
            };
            crate::actions::add(c, &r, &old, "agentflow")?;
            let mut conflicting = pr(48, "main", "b48");
            conflicting.mergeable = "conflicting".into();
            let (_, done) = sync(c, &p, "o/r", &conflicting, "poller")?;
            assert!(crate::actions::for_record(c, r.id)?.is_empty(), "withdrawn");
            // GitHub calling it conflicting tells the agent once.
            assert!(done.conflicted);
            let (_, again) = sync(c, &p, "o/r", &conflicting, "poller")?;
            assert!(!again.conflicted, "once");
            let events = records::recent_events(c, Some(p.id), 10)?;
            assert!(
                events
                    .iter()
                    .any(|e| e.kind == "pr.conflicting"
                        && e.summary == "#48 has conflicts with main: merge main into it"),
                "{events:?}"
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn sync_and_merge_order() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let p = crate::projects::create(c, "v", "V", "t")?;
            // A stack 48 ← 49 ← 50, and an independent 51 on main.
            for (n, base, head) in [(49, "b48", "b49"), (48, "main", "b48"), (50, "b49", "b50"), (51, "main", "b51")] {
                let (r, res) = sync(c, &p, "o/r", &pr(n, base, head), "poller")?;
                assert!(res.created);
                assert_eq!(r.ref_id, format!("v/PR-{n}"));
            }
            let order: Vec<(i64, Option<i64>)> =
                merge_order(c, p.id)?.iter().map(|o| (o.record.number, o.after)).collect();
            assert_eq!(order, [(48, None), (49, Some(48)), (50, Some(49)), (51, None)]);

            // Ready once it isn't a draft on GitHub (the only say in it, D-233); then CI and conflicts decide.
            let r48 = records::find_str(c, "v/PR-48", None)?;
            assert!(merge_ready(&r48).is_ok());
            let mut draft = pr(48, "main", "b48");
            draft.draft = true;
            let (r48, _) = sync(c, &p, "o/r", &draft, "poller")?;
            assert_eq!(merge_ready(&r48).unwrap_err(), "still a draft");
            let (r48, _) = sync(c, &p, "o/r", &pr(48, "main", "b48"), "poller")?;
            assert!(merge_ready(&r48).is_ok());
            let mut failing = pr(48, "main", "b48");
            failing.ci = "failing".into();
            let (r48, res) = sync(c, &p, "o/r", &failing, "poller")?;
            assert!(res.ci_failed);
            assert_eq!(merge_ready(&r48).unwrap_err(), "CI failing");
            let (_, res) = sync(c, &p, "o/r", &pr(48, "main", "b48"), "poller")?;
            assert!(res.became_mergeable && !res.ci_failed);

            let mut merged = pr(48, "main", "b48");
            merged.state = "merged".into();
            let (r, res) = sync(c, &p, "o/r", &merged, "poller")?;
            assert!(res.merged && r.state == "merged");
            let order: Vec<i64> = merge_order(c, p.id)?.iter().map(|o| o.record.number).collect();
            assert_eq!(order, [49, 50, 51]);
            Ok(())
        })
        .unwrap();
    }
}
