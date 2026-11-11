//! PRs and issues on other people's repositories, kept as records of the project they came from
//! and watched: a new comment, a merge or a close is told to that project's agent. (This replaces
//! the consultant's external-watch timer once the agents move over.)

use rusqlite::Connection;
use serde_json::{Map, Value};

use crate::github::ItemInfo;
use crate::records::{self, Kind, NewRecord, Patch, Query, Record};

/// The state of one still open on GitHub, so still watched.
const WATCHING: &str = "sent";

/// The record's state for what GitHub says, when it's finished there; None while it's open.
fn finished_state(github_state: &str) -> Option<&'static str> {
    match github_state {
        "merged" => Some("merged"),
        "closed" => Some("closed"),
        _ => None,
    }
}

/// Most contributions a project can have watched at once: each costs GitHub calls every round.
pub const MAX_WATCHED: usize = 50;

/// Refuses a new one when the project watches MAX_WATCHED already.
pub fn check_room(conn: &Connection, project_id: i64) -> anyhow::Result<()> {
    let open = records::query(
        conn,
        &Query {
            project_id: Some(project_id),
            kinds: vec![Kind::Contribution],
            states: vec![WATCHING.into()],
            limit: 0,
            ..Default::default()
        },
    )?
    .len();
    if open >= MAX_WATCHED {
        anyhow::bail!("this project watches {open} contributions already; that's the most at once");
    }
    Ok(())
}

/// Starts watching `repo#number` for a project. Idempotent.
pub fn track(
    conn: &Connection,
    project_id: i64,
    repo: &str,
    number: i64,
    info: &ItemInfo,
    author: &str,
) -> anyhow::Result<Record> {
    let existing = records::query(
        conn,
        &Query { project_id: Some(project_id), kinds: vec![Kind::Contribution], ..Default::default() },
    )?
    .into_iter()
    .find(|r| r.str("repo") == repo && r.data.get("number").and_then(Value::as_i64) == Some(number));
    if let Some(r) = existing {
        return Ok(r);
    }
    check_room(conn, project_id)?;
    let mut data = Map::new();
    data.insert("repo".into(), repo.into());
    data.insert("number".into(), number.into());
    data.insert("url".into(), info.url.clone().into());
    data.insert("type".into(), if info.is_pr { "pr" } else { "issue" }.into());
    data.insert("comments".into(), info.comments.into());
    let state = finished_state(&info.state).unwrap_or(WATCHING);
    records::create(
        conn,
        project_id,
        author,
        NewRecord {
            kind: Some(Kind::Contribution),
            title: format!("{repo}#{number}: {}", info.title),
            state: Some(state.into()),
            data,
            ..Default::default()
        },
    )
}

/// Applies what GitHub says now; returns what to tell the agent, if anything changed.
pub fn update(conn: &Connection, r: &Record, info: &ItemInfo) -> anyhow::Result<Option<String>> {
    let before = r.data.get("comments").and_then(Value::as_i64).unwrap_or(0);
    let state = finished_state(&info.state).unwrap_or(r.state.as_str());
    let mut news = Vec::new();
    if info.comments > before {
        let n = info.comments - before;
        news.push(format!("{n} new comment{}", if n == 1 { "" } else { "s" }));
    }
    if state != r.state {
        news.push(format!("now {state}"));
    }
    if news.is_empty() {
        return Ok(None);
    }
    let mut data = Map::new();
    data.insert("comments".into(), info.comments.into());
    let (r, _) = records::update(
        conn,
        r,
        "github",
        Patch { state: Some(state.into()), data, quiet: true, ..Default::default() },
    )?;
    let what = format!("{}: {} ({})", r.title, news.join(", "), r.str("url"));
    records::event(conn, Some(r.project_id), Some(r.id), "contribution.updated", &what, "github")?;
    Ok(Some(what))
}

/// Contributions still worth watching: not merged or closed.
pub fn watched(conn: &Connection) -> anyhow::Result<Vec<Record>> {
    records::query(
        conn,
        &Query { kinds: vec![Kind::Contribution], states: vec![WATCHING.into()], ..Default::default() },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn item(state: &str, comments: i64) -> ItemInfo {
        ItemInfo {
            title: "Undo leaves content".into(),
            url: "https://github.com/loro-dev/loro/issues/1255".into(),
            is_pr: false,
            state: state.into(),
            comments,
        }
    }

    #[test]
    fn watching_reports_comments_and_closes() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let p = crate::projects::create(c, "imaginarium", "Imaginarium", "t")?;
            let r = track(c, p.id, "loro-dev/loro", 1255, &item("open", 0), "consultant")?;
            assert_eq!(track(c, p.id, "loro-dev/loro", 1255, &item("open", 0), "consultant")?.id, r.id, "once");
            assert_eq!(update(c, &r, &item("open", 0))?, None);
            let news = update(c, &r, &item("open", 2))?.unwrap();
            assert!(news.contains("2 new comments"), "{news}");
            let r = records::get(c, r.id)?;
            let news = update(c, &r, &item("closed", 2))?.unwrap();
            assert!(news.contains("now closed"));
            assert!(watched(c)?.is_empty());
            let events = crate::waiting::unread(c, p.id, "imaginarium")?;
            assert_eq!(events.iter().filter(|e| e.kind == "contribution.updated").count(), 2);
            // Only so many are watched at once; closed ones don't count.
            for n in 0..MAX_WATCHED as i64 {
                track(c, p.id, "loro-dev/loro", 2000 + n, &item("open", 0), "consultant")?;
            }
            assert!(track(c, p.id, "loro-dev/loro", 9999, &item("open", 0), "consultant").is_err());
            Ok(())
        })
        .unwrap();
    }
}
