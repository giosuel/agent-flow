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
    if !pr.data.get("ready").and_then(Value::as_bool).unwrap_or(false) {
        return Err("the agent hasn't marked it ready".into());
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

            // Not ready until the agent says so; then CI and conflicts decide.
            let r48 = records::find_str(c, "v/PR-48", None)?;
            assert!(merge_ready(&r48).is_err());
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
