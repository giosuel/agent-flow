//! Pull requests: kept in step with GitHub by a poller, put in merge order from their bases, and
//! marked ready by their project's agent.

use std::collections::HashMap;

use rusqlite::Connection;
use serde_json::{Map, Value, json};

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

/// The command a PR's Merge button runs: the project's merge method, deleting the branch so
/// GitHub moves the next PR of a stack onto main.
pub fn merge_command(repo: &str, number: i64, method: &str) -> String {
    let method = if ["merge", "squash", "rebase"].contains(&method) { method } else { "merge" };
    format!("gh pr merge {number} -R {repo} --{method} --delete-branch")
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
        "SELECT id FROM records WHERE project_id = ?1 AND kind = 'task' AND json_extract(data, '$.branch') = ?2
         AND id NOT IN (SELECT to_id FROM links WHERE from_id = ?3 AND relation = 'implements')",
    )?;
    let ids: Vec<i64> = stmt.query_map(rusqlite::params![project_id, head, pr.id], |r| r.get(0))?.collect::<Result<_, _>>()?;
    for id in ids {
        let t = records::get(conn, id)?;
        records::link(conn, pr, &t, "implements", author)?;
    }
    Ok(())
}

/// Why a PR's own Merge button is disabled, or Ok when it can be pressed.
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
    // Ready unless the agent said it isn't: GitHub's draft flag says the rest (Imaginarium's agent,
    // 2026-11-12: "a non-draft PR is ready and a draft isn't").
    if pr.data.get("ready").and_then(Value::as_bool) == Some(false) {
        return Err("the agent marked it not ready".into());
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
        out.push(Ordered { record: r.clone(), position: out.len() + 1, after: parent(r) });
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
            out.push(Ordered { record: r.clone(), position: out.len() + 1, after: parent(r) });
        }
    }
    Ok(out)
}

/// The agent marks its PR ready for the owner (or not), optionally saying which tasks it carries out.
pub fn set_ready(conn: &Connection, pr: &Record, ready: bool, note: &str, author: &str) -> anyhow::Result<Record> {
    let mut data = Map::new();
    data.insert("ready".into(), json!(ready));
    let (r, _) = records::update(conn, pr, author, Patch { data, note: note.into(), ..Default::default() })?;
    Ok(r)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::github::fake::pr;

    #[test]
    fn every_pr_of_the_repo_gets_its_merge_button() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let p = crate::projects::create(c, "v", "V", "t")?;
            let patch = crate::projects::ProjectPatch { repo: Some("o/r".into()), merge_method: Some("squash".into()), ..Default::default() };
            crate::projects::update(c, p.id, &patch, "owner")?;
            assert!(crate::projects::update(c, p.id, &crate::projects::ProjectPatch { merge_method: Some("yolo".into()), ..Default::default() }, "o").is_err());
            let p = crate::projects::get_by_id(c, p.id)?;
            let (r, _) = sync(c, &p, "o/r", &pr(48, "main", "b48"), "poller")?;
            sync(c, &p, "o/r", &pr(48, "main", "b48"), "poller")?;
            let buttons = crate::actions::for_record(c, r.id)?;
            assert_eq!(buttons.len(), 1, "once, however often it syncs");
            assert_eq!(buttons[0].command, "gh pr merge 48 -R o/r --squash --delete-branch");
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
            let live: Vec<_> = crate::actions::for_record(c, r.id)?.into_iter().filter(|b| b.state != "withdrawn").collect();
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

            // Ready once it isn't a draft, unless the agent says it isn't; then CI and conflicts decide.
            let r48 = records::find_str(c, "v/PR-48", None)?;
            assert!(merge_ready(&r48).is_ok());
            let r48 = set_ready(c, &r48, false, "waiting for the review agent", "v")?;
            assert_eq!(merge_ready(&r48).unwrap_err(), "the agent marked it not ready");
            let r48 = set_ready(c, &r48, true, "ready for review", "v")?;
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
            assert_eq!(r.data["ready"], true, "the agent's fields survive a sync");
            let order: Vec<i64> = merge_order(c, p.id)?.iter().map(|o| o.record.number).collect();
            assert_eq!(order, [49, 50, 51]);
            Ok(())
        })
        .unwrap();
    }
}
