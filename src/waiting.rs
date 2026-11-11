//! What's waiting on the owner, across all projects: the Dashboard, the phone inbox and the
//! count in the navigation all come from here.
//!
//! The owner, 2026-11-11: merges "don't really matter that much. we could keep a separate count
//! for that". So merges and releases are listed but not counted in the badge.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::records::{self, Event, Kind, Query, Record};
use crate::util::now_ms;

pub struct Waiting {
    /// Open questions, the one to answer first at the front.
    pub questions: Vec<Record>,
    /// PRs and releases with a button for the owner (M4).
    pub merges: Vec<Record>,
    /// Builds published for the owner to try (M5).
    pub builds: Vec<Record>,
}

pub fn load(conn: &Connection) -> anyhow::Result<Waiting> {
    let mut questions = records::query(
        conn,
        &Query { kinds: vec![Kind::Question], states: vec!["open".into()], limit: 0, ..Default::default() },
    )?;
    // Urgent first, then questions that block something, then the oldest.
    let blocking = blocking_ids(conn)?;
    questions.sort_by_key(|q| {
        let urgent = q.data.get("urgent").and_then(Value::as_bool).unwrap_or(false);
        (!urgent, !blocking.contains(&q.id), q.created_at)
    });
    // PRs that can be merged now (ready, and not waiting for a PR they're based on), in each
    // project's merge order; then releases whose PRs are all merged.
    let mut merges: Vec<Record> = Vec::new();
    let mut with_prs: Vec<i64> =
        records::query(conn, &Query { kinds: vec![Kind::Pr], states: vec!["open".into()], ..Default::default() })?
            .iter()
            .map(|r| r.project_id)
            .collect();
    with_prs.sort_unstable();
    with_prs.dedup();
    for project_id in with_prs {
        for o in crate::prs::merge_order(conn, project_id)? {
            if o.after.is_none() && crate::prs::merge_ready(&o.record).is_ok() {
                merges.push(o.record);
            }
        }
    }
    for r in records::query(
        conn,
        &Query { kinds: vec![Kind::Release], states: vec!["waiting".into()], ..Default::default() },
    )? {
        if crate::prs::release_ready(conn, &r)? {
            merges.push(r);
        }
    }
    let builds = records::query(
        conn,
        &Query { kinds: vec![Kind::Build], states: vec!["published".into()], limit: 0, ..Default::default() },
    )?
    .into_iter()
    .filter(|r| r.data.get("ready_for_feedback").and_then(Value::as_bool).unwrap_or(false))
    .collect();
    // An archived project waits for nothing.
    let archived: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM projects WHERE status = 'archived'")?;
        stmt.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?
    };
    let mut builds: Vec<Record> = builds;
    for list in [&mut questions, &mut merges, &mut builds] {
        list.retain(|r| !archived.contains(&r.project_id));
    }
    Ok(Waiting { questions, merges, builds })
}

/// Leaves out archived projects' records.
const LIVE: &str = "AND project_id NOT IN (SELECT id FROM projects WHERE status = 'archived')";

/// Just the badge number, for every page's navigation.
pub fn count(conn: &Connection) -> anyhow::Result<usize> {
    let questions: i64 = conn.query_row(
        &format!("SELECT count(*) FROM records WHERE kind = 'question' AND state = 'open' {LIVE}"),
        [],
        |r| r.get(0),
    )?;
    let builds: i64 = conn.query_row(
        &format!("SELECT count(*) FROM records WHERE kind = 'build' AND state = 'published' AND json_extract(data, '$.ready_for_feedback') {LIVE}"),
        [],
        |r| r.get(0),
    )?;
    Ok((questions + builds) as usize)
}

fn blocking_ids(conn: &Connection) -> anyhow::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT DISTINCT from_id FROM links WHERE relation = 'blocks'")?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// What a question blocks, by name and page, for "Blocks: Sidebar layout".
pub fn blocks(conn: &Connection, q: &Record) -> anyhow::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT to_id FROM links WHERE from_id = ?1 AND relation = 'blocks' ORDER BY id")?;
    let ids: Vec<i64> = stmt.query_map([q.id], |r| r.get(0))?.collect::<Result<_, _>>()?;
    let mut out = Vec::new();
    for id in ids {
        let r = records::get(conn, id)?;
        out.push((r.title.clone(), r.url()));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// The agent's side: what happened in its project since it last looked.

/// Events in the project that someone else caused (the owner answering, another agent, the
/// consultant, AgentFlow itself), after the agent's cursor.
/// Returns at most `UNREAD_PAGE`; the caller moves the cursor to the last one it handed out.
pub fn unread(conn: &Connection, project_id: i64, slug: &str) -> anyhow::Result<Vec<Event>> {
    let after = cursor(conn, project_id)?;
    records::events_by_others(conn, project_id, slug, after, UNREAD_PAGE)
}

pub const UNREAD_PAGE: i64 = 200;

/// How many unread events there are, without a cap.
pub fn unread_count(conn: &Connection, project_id: i64, slug: &str) -> anyhow::Result<i64> {
    let after = cursor(conn, project_id)?;
    Ok(conn.query_row(
        "SELECT count(*) FROM events WHERE project_id = ?1 AND seq > ?2 AND actor != ?3",
        params![project_id, after, slug],
        |r| r.get(0),
    )?)
}

pub fn cursor(conn: &Connection, project_id: i64) -> anyhow::Result<i64> {
    let c: Option<i64> =
        conn.query_row("SELECT last_seq FROM cursors WHERE project_id = ?1", [project_id], |r| r.get(0)).optional()?;
    match c {
        Some(c) => Ok(c),
        // A new agent starts from now, not from the project's whole history.
        None => {
            let latest = latest_seq(conn)?;
            set_cursor(conn, project_id, latest)?;
            Ok(latest)
        }
    }
}

pub fn set_cursor(conn: &Connection, project_id: i64, seq: i64) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO cursors (project_id, last_seq, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (project_id) DO UPDATE SET last_seq = max(last_seq, excluded.last_seq), updated_at = excluded.updated_at",
        params![project_id, seq, now_ms()],
    )?;
    Ok(())
}

/// The newest event seq overall; where a new agent starts reading.
pub fn latest_seq(conn: &Connection) -> anyhow::Result<i64> {
    Ok(conn.query_row("SELECT coalesce(max(seq), 0) FROM events", [], |r| r.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::records::NewRecord;
    use serde_json::{Map, json};

    #[test]
    fn urgent_and_blocking_questions_come_first() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let p = crate::projects::create(c, "v", "V", "t")?.id;
            let q = |title: &str, urgent: bool| {
                let mut data = Map::new();
                if urgent {
                    data.insert("urgent".into(), json!(true));
                }
                records::create(
                    c,
                    p,
                    "v",
                    NewRecord { kind: Some(Kind::Question), title: title.into(), data, ..Default::default() },
                )
            };
            let _old = q("old", false)?;
            let blocker = q("blocker", false)?;
            let urgent = q("urgent", true)?;
            let task = records::create(
                c,
                p,
                "v",
                NewRecord { kind: Some(Kind::Task), title: "t".into(), ..Default::default() },
            )?;
            records::link(c, &blocker, &task, "blocks", "v")?;
            let w = load(c)?;
            let order: Vec<_> = w.questions.iter().map(|q| q.title.as_str()).collect();
            assert_eq!(order, ["urgent", "blocker", "old"]);
            assert_eq!(count(c)?, 3);
            assert_eq!(blocks(c, &blocker)?, [("t".to_owned(), "/p/v/T-1".to_owned())]);
            let _ = urgent;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn unread_skips_own_events_and_starts_now() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let p = crate::projects::create(c, "v", "V", "admin")?.id;
            assert!(unread(c, p, "v")?.is_empty(), "history before the first look isn't unread");
            records::event(c, Some(p), None, "x", "agent did something", "v")?;
            let seq = records::event(c, Some(p), None, "question.answered", "Owner answered Q-1", "owner")?;
            let u = unread(c, p, "v")?;
            assert_eq!(u.len(), 1);
            set_cursor(c, p, seq)?;
            assert!(unread(c, p, "v")?.is_empty());
            set_cursor(c, p, 1)?;
            assert_eq!(cursor(c, p)?, seq, "cursors never move back");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn many_own_events_dont_hide_the_owners() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let p = crate::projects::create(c, "v", "V", "admin")?.id;
            cursor(c, p)?;
            for i in 0..250 {
                records::event(c, Some(p), None, "task.created", &format!("own {i}"), "v")?;
            }
            records::event(c, Some(p), None, "question.answered", "Owner answered Q-1", "owner")?;
            let u = unread(c, p, "v")?;
            assert_eq!(u.len(), 1);
            assert_eq!(unread_count(c, p, "v")?, 1);
            for i in 0..(UNREAD_PAGE + 5) {
                records::event(c, Some(p), None, "x", &format!("theirs {i}"), "owner")?;
            }
            let page = unread(c, p, "v")?;
            assert_eq!(page.len() as i64, UNREAD_PAGE);
            set_cursor(c, p, page.last().unwrap().seq)?;
            assert_eq!(unread(c, p, "v")?.len(), 6, "the rest stays unread, nothing is skipped");
            Ok(())
        })
        .unwrap();
    }
}
