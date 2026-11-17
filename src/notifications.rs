//! The bell at the top (the owner, 2026-11-17: "a notification overlay that stores incoming
//! notifications … like youtube, so when the user opens the notifications on the desktop or phone,
//! they are all marked as read and the badge goes away"). Every notification AgentFlow sends is
//! kept here, whether or not a device got it as a push; they're kept for 30 days.

use rusqlite::{Connection, params};
use serde::Serialize;

use crate::util::now_ms;

pub const KEEP_DAYS: i64 = 30;

#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub id: i64,
    pub at: i64,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub url: String,
    pub unread: bool,
}

/// Keeps a notification. One with a tag replaces an unread one with the same tag (as pushes do).
pub fn store(c: &Connection, kind: &str, title: &str, body: &str, url: &str, tag: Option<&str>) -> anyhow::Result<()> {
    let now = now_ms();
    if let Some(tag) = tag {
        let n = c.execute(
            "UPDATE notifications SET at = ?2, kind = ?3, title = ?4, body = ?5, url = ?6 WHERE tag = ?1 AND read_at IS NULL",
            params![tag, now, kind, title, body, url],
        )?;
        if n > 0 {
            return Ok(());
        }
    }
    c.execute(
        "INSERT INTO notifications (at, kind, title, body, url, tag) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![now, kind, title, body, url, tag],
    )?;
    Ok(())
}

pub fn unread(c: &Connection) -> anyhow::Result<i64> {
    Ok(c.query_row("SELECT count(*) FROM notifications WHERE read_at IS NULL AND at > ?1", [cutoff()], |r| r.get(0))?)
}

/// The newest first, those of the last 30 days.
pub fn list(c: &Connection, limit: i64) -> anyhow::Result<Vec<Item>> {
    let mut st = c.prepare(
        "SELECT id, at, kind, title, body, url, read_at IS NULL FROM notifications WHERE at > ?1 ORDER BY at DESC, id DESC LIMIT ?2",
    )?;
    let rows = st.query_map(params![cutoff(), limit], |r| {
        Ok(Item {
            id: r.get(0)?,
            at: r.get(1)?,
            kind: r.get(2)?,
            title: r.get(3)?,
            body: r.get(4)?,
            url: r.get(5)?,
            unread: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Opening the bell reads them all.
pub fn mark_all_read(c: &Connection) -> anyhow::Result<usize> {
    Ok(c.execute("UPDATE notifications SET read_at = ?1 WHERE read_at IS NULL", [now_ms()])?)
}

/// Forgets those older than 30 days.
pub fn purge(c: &Connection) -> anyhow::Result<usize> {
    Ok(c.execute("DELETE FROM notifications WHERE at <= ?1", [cutoff()])?)
}

fn cutoff() -> i64 {
    now_ms() - KEEP_DAYS * 86_400_000
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn kept_replaced_by_tag_and_read_all_at_once() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            store(c, "question", "Q-1 asked", "", "/p/x/Q-1", Some("q1"))?;
            store(c, "question", "Q-1 asked again", "", "/p/x/Q-1", Some("q1"))?;
            store(c, "build", "Build ready", "", "", None)?;
            assert_eq!(unread(c)?, 2, "the tag replaced the unread one");
            assert_eq!(list(c, 10)?[1].title, "Q-1 asked again");
            assert_eq!(mark_all_read(c)?, 2);
            assert_eq!(unread(c)?, 0);
            assert!(list(c, 10)?.iter().all(|i| !i.unread));
            c.execute("UPDATE notifications SET at = 0", [])?;
            assert_eq!(purge(c)?, 2);
            Ok(())
        })
        .unwrap();
    }
}
