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
        ensure_merge_button(conn, project, repo, &r)?;
        return Ok((r, SyncResult { created: true, merged: state == "merged", ..Default::default() }));
    };
    let was_ready = merge_ready(&old).is_ok();
    let mut result = SyncResult {
        merged: state == "merged" && old.state != "merged",
        closed: state == "closed" && old.state != "closed",
        ci_failed: info.ci == "failing" && old.str("ci") != "failing",
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
    ensure_merge_button(conn, project, repo, &r)?;
    // The agent hears about what GitHub did to its PR.
    for (happened, kind, what) in [
        (result.merged, "pr.merged", format!("#{} merged: {}", r.number, r.title)),
        (result.closed, "pr.closed", format!("#{} closed without merging", r.number)),
        (result.ci_failed, "pr.ci_failed", format!("CI failing on #{}", r.number)),
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

/// Is this exactly a command AgentFlow makes itself (a Merge or Push tag button's)? Only those
/// run with the server's GitHub token; a command an agent wrote never sees it.
pub fn is_server_command(cmd: &str) -> bool {
    let safe = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-+/".contains(c));
    let w: Vec<&str> = cmd.split(' ').collect();
    if let ["gh", "pr", "merge", n, "-R", repo, method, "--delete-branch"] = w.as_slice() {
        let m = method.trim_start_matches("--");
        return n.parse::<i64>().is_ok()
            && crate::projects::looks_like_repo(repo)
            && safe(repo)
            && cmd == merge_command(repo, n.parse().unwrap_or(0), m)
            && ["merge", "squash", "rebase"].contains(&m);
    }
    let between = |a: &str, b: &str| cmd.split_once(a).and_then(|(_, r)| r.split_once(b)).map(|(x, _)| x.to_owned());
    // The tag from the last line, after the message.
    let tag = cmd.rsplit_once(" -f ref=refs/tags/").and_then(|(_, r)| r.split_once(' ')).map(|(t, _)| t.to_owned());
    let (Some(repo), Some(tag)) = (between("gh api repos/", "/git/ref/heads/main"), tag) else {
        return false;
    };
    if !(crate::projects::looks_like_repo(&repo) && safe(&repo) && safe(&tag)) {
        return false;
    }
    if cmd == tag_command_bare(&repo, &tag) {
        return true;
    }
    // The message is whatever stands between the heredoc's lines; the rest must be exactly ours.
    let start = format!("<<'{MESSAGE_END}'\n");
    let end = format!("\n{MESSAGE_END}\n)");
    match cmd.split_once(&start).and_then(|(_, r)| r.split_once(&end)) {
        Some((message, _)) => check_tag_message(message).is_ok() && cmd == tag_command(&repo, &tag, message),
        None => false,
    }
}

/// Every open PR of the project's own repository has its Merge button (Imaginarium's agent,
/// 2026-11-12: "a 6-PR stack still means 6 add_button calls"). It unlocks when the PR is ready and
/// in merge order; pressing it is the owner's approval. One the agent added itself counts too.
fn ensure_merge_button(conn: &Connection, project: &Project, repo: &str, pr: &Record) -> anyhow::Result<()> {
    if pr.state != "open" || repo.is_empty() || !repo.eq_ignore_ascii_case(project.repo.trim()) {
        return Ok(());
    }
    let has: i64 = conn.query_row(
        "SELECT count(*) FROM actions WHERE record_id = ?1 AND kind = 'merge' AND state != 'withdrawn'",
        [pr.id],
        |r| r.get(0),
    )?;
    if has > 0 {
        return Ok(());
    }
    crate::actions::add(
        conn,
        pr,
        &crate::actions::NewButton {
            label: "Merge",
            kind: crate::actions::ButtonKind::parse("merge")?,
            command: &merge_command(repo, pr.number, &project.merge_method),
            workdir: "",
            requires: &[],
        },
        "agentflow",
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

/// Why a PR's own Merge button is disabled, or Ok when it can be pressed.
pub fn merge_ready(pr: &Record) -> Result<(), String> {
    ready_with(pr, true)
}

/// Ready to merge as a step inside "Merge #a–#b" (the owner, 2026-11-18, D-215): failing or
/// running CI doesn't hold a PR in the middle of a stack, as long as the PR the chain ends at
/// passes, so main ends up green.
pub fn chain_ready(pr: &Record) -> Result<(), String> {
    ready_with(pr, false)
}

fn ready_with(pr: &Record, ci_matters: bool) -> Result<(), String> {
    if pr.state != "open" {
        return Err(format!("already {}", pr.state));
    }
    // Problems on GitHub first: they say more than "not marked ready".
    if pr.data.get("draft").and_then(Value::as_bool).unwrap_or(false) {
        return Err("still a draft".into());
    }
    if ci_matters && pr.str("ci") == "failing" {
        return Err("CI failing".into());
    }
    if pr.str("mergeable") == "conflicting" {
        return Err("has conflicts".into());
    }
    if ci_matters && pr.str("ci") == "pending" {
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

/// One step of "Merge #a–#b": a PR and its merge button.
pub struct ChainStep {
    pub number: i64,
    pub action: crate::actions::ActionRow,
}

/// What "Merge #a–#b" merges to bring stacked PR `number` onto main: every PR from the bottom of
/// its stack up to it, bottom first, each with its merge button; or why it can't (the owner,
/// 2026-11-18, D-214: "i dont really want the user being able to mess up anything"). A stacked
/// PR merges only this way. The PRs below need to be ready apart from CI; the PR it ends at needs
/// passing CI too, so main ends up green (D-215).
pub fn chain_to(conn: &Connection, order: &[Ordered], number: i64) -> anyhow::Result<Result<Vec<ChainStep>, String>> {
    let by_number: HashMap<i64, &Ordered> = order.iter().map(|o| (o.record.number, o)).collect();
    let Some(&top) = by_number.get(&number) else { return Ok(Err(format!("#{number} isn't open"))) };
    if top.after.is_none() {
        return Ok(Err("it isn't stacked: it has its own Merge".into()));
    }
    let mut stack = vec![top];
    while let Some(after) = stack[stack.len() - 1].after {
        let Some(&below) = by_number.get(&after) else { return Ok(Err(format!("#{after} isn't open"))) };
        if stack.len() > order.len() {
            return Ok(Err("its stack goes round in a circle".into()));
        }
        stack.push(below);
    }
    stack.reverse();
    if let Err(why) = merge_ready(&top.record) {
        return Ok(Err(why));
    }
    let bottom = &stack[0].record;
    if !stack[0].base_ok {
        return Ok(Err(format!(
            "#{}: based on {}, which isn't main and has no open PR",
            bottom.number,
            bottom.str("base")
        )));
    }
    let mut steps = Vec::new();
    for o in stack {
        let n = o.record.number;
        let mine = |why: String| if n == number { why } else { format!("#{n}: {why}") };
        let button = crate::actions::for_record(conn, o.record.id)?
            .into_iter()
            .find(|a| a.is_merge() && matches!(a.state.as_str(), "waiting" | "failed"));
        let Some(a) = button else { return Ok(Err(mine("no Merge button yet".into()))) };
        if let Err(why) = crate::actions::enabled_in_chain(conn, &a)? {
            return Ok(Err(mine(why)));
        }
        steps.push(ChainStep { number: n, action: a });
    }
    Ok(Ok(steps))
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
    fn every_pr_of_the_repo_gets_its_merge_button() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let p = crate::projects::create(c, "v", "V", "t")?;
            let patch = crate::projects::ProjectPatch {
                repo: Some("o/r".into()),
                merge_method: Some("squash".into()),
                ..Default::default()
            };
            crate::projects::update(c, p.id, &patch, "owner")?;
            assert!(
                crate::projects::update(
                    c,
                    p.id,
                    &crate::projects::ProjectPatch { merge_method: Some("yolo".into()), ..Default::default() },
                    "o"
                )
                .is_err()
            );
            let p = crate::projects::get_by_id(c, p.id)?;
            let (r, _) = sync(c, &p, "o/r", &pr(48, "main", "b48"), "poller")?;
            sync(c, &p, "o/r", &pr(48, "main", "b48"), "poller")?;
            let buttons = crate::actions::for_record(c, r.id)?;
            assert_eq!(buttons.len(), 1, "once, however often it syncs");
            assert_eq!(buttons[0].command, "gh pr merge 48 -R o/r --squash --delete-branch");
            // A new merge method reaches the waiting buttons.
            crate::projects::update(
                c,
                p.id,
                &crate::projects::ProjectPatch { merge_method: Some("merge".into()), ..Default::default() },
                "owner",
            )?;
            assert_eq!(
                crate::actions::for_record(c, r.id)?[0].command,
                "gh pr merge 48 -R o/r --merge --delete-branch"
            );
            assert!(buttons[0].is_merge());
            // Another repository's PR gets none; the agent's own button replaces AgentFlow's.
            let (other, _) = sync(c, &p, "x/y", &pr(7, "main", "b7"), "poller")?;
            assert!(crate::actions::for_record(c, other.id)?.is_empty());
            let mine = crate::actions::NewButton {
                label: "Merge",
                kind: crate::actions::ButtonKind::parse("merge")?,
                command: "gh pr merge 48 -R o/r --merge",
                workdir: "",
                requires: &[],
            };
            crate::actions::add(c, &r, &mine, "v")?;
            let live: Vec<_> =
                crate::actions::for_record(c, r.id)?.into_iter().filter(|b| b.state != "withdrawn").collect();
            assert_eq!(live.len(), 1);
            assert_eq!(live[0].command, "gh pr merge 48 -R o/r --merge");
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
