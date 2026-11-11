//! Conversations: agents, the consultant and the owner writing to each other in threads. Every
//! message lands in the recipients' event feeds (so `wait` and `since_last_check` deliver it) and
//! is kept for the Conversations page and the log.

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::activity::{clip, redact};
use crate::records::event;
use crate::util::now_ms;

pub const KINDS: &[&str] = &["info", "question", "answer", "resource", "handover"];
pub const CONSULTANT: &str = "consultant";
pub const OWNER: &str = "owner";
pub const ALL: &str = "all";

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub id: i64,
    pub thread_id: i64,
    pub sender: String,
    pub kind: String,
    pub body: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Thread {
    pub id: i64,
    pub subject: String,
    pub started_by: String,
    pub members: Vec<String>,
    pub created_at: i64,
    pub last_at: i64,
    pub count: i64,
    /// The newest message's kind, for the list.
    pub last_kind: String,
}

/// A recipient: a project's slug, "consultant", or "all" (every project).
fn check_member(conn: &Connection, m: &str) -> anyhow::Result<()> {
    if m == CONSULTANT || m == ALL || m == OWNER {
        return Ok(());
    }
    crate::projects::get(conn, m)?
        .with_context(|| format!("no project {m:?}; write to a project's slug, \"consultant\" or \"all\""))?;
    Ok(())
}

/// Sends a message, starting a thread or adding to one. Returns (thread id, message id).
pub fn send(
    conn: &Connection,
    sender: &str,
    to: &[String],
    subject: &str,
    body: &str,
    kind: &str,
    thread: Option<i64>,
) -> anyhow::Result<(i64, i64)> {
    if !KINDS.contains(&kind) {
        bail!("kind must be one of: {}", KINDS.join(", "));
    }
    let body = redact(body.trim());
    if body.is_empty() {
        bail!("the message is empty");
    }
    if body.chars().count() > 20_000 {
        bail!("keep a message under 20,000 characters");
    }
    let now = now_ms();
    let thread_id = match thread {
        Some(id) => {
            let members = members(conn, id)?;
            if members.is_empty() {
                bail!("no conversation {id}");
            }
            let is_member = members.iter().any(|m| m == sender || m == ALL) || sender == OWNER || sender == CONSULTANT;
            if !is_member {
                bail!("you aren't part of conversation {id}");
            }
            // Someone new can be brought in, by whoever started it (or the consultant or owner):
            // otherwise any member could pull every project into a thread.
            let started_by: String =
                conn.query_row("SELECT started_by FROM threads WHERE id = ?1", [id], |r| r.get(0))?;
            let adds = !members.iter().any(|m| m == ALL) && to.iter().any(|m| !members.contains(m));
            if adds && sender != started_by && sender != OWNER && sender != CONSULTANT {
                bail!("only {started_by} (who started it) can bring someone into conversation {id}");
            }
            for m in to {
                check_member(conn, m)?;
                conn.execute(
                    "INSERT OR IGNORE INTO thread_members (thread_id, member) VALUES (?1, ?2)",
                    params![id, m],
                )?;
            }
            conn.execute("UPDATE threads SET last_at = ?1 WHERE id = ?2", params![now, id])?;
            id
        }
        None => {
            if to.is_empty() {
                bail!("say who it's for: a project's slug, \"consultant\" or \"all\"");
            }
            let subject = subject.trim();
            if subject.is_empty() {
                bail!("a new conversation needs a subject");
            }
            for m in to {
                check_member(conn, m)?;
            }
            conn.execute(
                "INSERT INTO threads (subject, started_by, created_at, last_at) VALUES (?1, ?2, ?3, ?3)",
                params![clip(&redact(subject), 200), sender, now],
            )?;
            let id = conn.last_insert_rowid();
            for m in to.iter().map(String::as_str).chain(std::iter::once(sender)) {
                conn.execute(
                    "INSERT OR IGNORE INTO thread_members (thread_id, member) VALUES (?1, ?2)",
                    params![id, m],
                )?;
            }
            id
        }
    };
    conn.execute(
        "INSERT INTO messages (thread_id, sender, kind, body, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![thread_id, sender, kind, body, now],
    )?;
    let msg_id = conn.last_insert_rowid();

    // Tell every project in the thread except the sender, through its event feed.
    let subject: String = conn.query_row("SELECT subject FROM threads WHERE id = ?1", [thread_id], |r| r.get(0))?;
    let summary = format!("Message from {sender} in \"{subject}\" (conversation {thread_id}): {}", clip(&body, 160));
    for project_id in recipient_projects(conn, thread_id, sender)? {
        event(conn, Some(project_id), None, "message", &summary, sender)?;
    }
    Ok((thread_id, msg_id))
}

/// The projects to notify: members, or every active project for "all", never the sender.
fn recipient_projects(conn: &Connection, thread_id: i64, sender: &str) -> anyhow::Result<Vec<i64>> {
    let members = members(conn, thread_id)?;
    let mut stmt = conn.prepare("SELECT id, slug FROM projects WHERE status = 'active'")?;
    let projects: Vec<(i64, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
    let all = members.iter().any(|m| m == ALL);
    Ok(projects
        .into_iter()
        .filter(|(_, slug)| slug != sender && (all || members.contains(slug)))
        .map(|(id, _)| id)
        .collect())
}

pub fn members(conn: &Connection, thread_id: i64) -> anyhow::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT member FROM thread_members WHERE thread_id = ?1 ORDER BY member")?;
    let rows = stmt.query_map([thread_id], |r| r.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

const MESSAGE_COLUMNS: &str = "m.id, m.thread_id, m.sender, m.kind, m.body, m.created_at";

fn message_from_row(r: &rusqlite::Row) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        thread_id: r.get(1)?,
        sender: r.get(2)?,
        kind: r.get(3)?,
        body: r.get(4)?,
        created_at: r.get(5)?,
    })
}

pub fn messages(conn: &Connection, thread_id: i64) -> anyhow::Result<Vec<Message>> {
    let mut stmt =
        conn.prepare(&format!("SELECT {MESSAGE_COLUMNS} FROM messages m WHERE m.thread_id = ?1 ORDER BY m.id"))?;
    let rows = stmt.query_map([thread_id], message_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn thread(conn: &Connection, id: i64) -> anyhow::Result<Option<Thread>> {
    Ok(threads(conn, None, 1, Some(id))?.into_iter().next())
}

/// Conversations, newest activity first; only those `member` takes part in (or "all") when given.
pub fn threads(conn: &Connection, member: Option<&str>, limit: i64, only: Option<i64>) -> anyhow::Result<Vec<Thread>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.subject, t.started_by, t.created_at, t.last_at,
                (SELECT count(*) FROM messages m WHERE m.thread_id = t.id),
                coalesce((SELECT kind FROM messages m WHERE m.thread_id = t.id ORDER BY m.id DESC LIMIT 1), 'info')
         FROM threads t
         WHERE (?1 IS NULL OR EXISTS (SELECT 1 FROM thread_members tm WHERE tm.thread_id = t.id AND tm.member IN (?1, 'all')))
           AND (?2 IS NULL OR t.id = ?2)
         ORDER BY t.last_at DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![member, only, limit], |r| {
        Ok(Thread {
            id: r.get(0)?,
            subject: r.get(1)?,
            started_by: r.get(2)?,
            created_at: r.get(3)?,
            last_at: r.get(4)?,
            count: r.get(5)?,
            last_kind: r.get(6)?,
            members: Vec::new(),
        })
    })?;
    let mut out: Vec<Thread> = rows.collect::<Result<_, _>>()?;
    for t in &mut out {
        t.members = members(conn, t.id)?;
    }
    Ok(out)
}

/// Messages to the consultant (or everyone) it hasn't marked read, oldest first. Reading doesn't
/// mark them: the consultant marks what it handled with `mark_consultant_read`, so a reply that
/// never arrived (a dropped connection, a failed tool call) doesn't lose them.
pub fn consultant_inbox(conn: &Connection) -> anyhow::Result<Vec<Message>> {
    let last: i64 = conn
        .query_row("SELECT last_id FROM consultant_reads WHERE reader = ?1", [CONSULTANT], |r| r.get(0))
        .optional()?
        .unwrap_or(0);
    let mut stmt = conn.prepare(&format!(
        "SELECT {MESSAGE_COLUMNS} FROM messages m
         WHERE m.id > ?1 AND m.sender != ?2
           AND EXISTS (SELECT 1 FROM thread_members tm WHERE tm.thread_id = m.thread_id AND tm.member IN (?2, 'all'))
         ORDER BY m.id LIMIT 200"
    ))?;
    let rows = stmt.query_map(params![last, CONSULTANT], message_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Marks the consultant's messages up to and including `up_to` read.
pub fn mark_consultant_read(conn: &Connection, up_to: i64) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO consultant_reads (reader, last_id) VALUES (?1, ?2)
         ON CONFLICT (reader) DO UPDATE SET last_id = max(last_id, excluded.last_id)",
        params![CONSULTANT, up_to],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn threads_reach_their_members() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let v = crate::projects::create(c, "vinculum", "Vinculum", "t")?.id;
            let a = crate::projects::create(c, "antimony", "Antimony", "t")?.id;
            let i = crate::projects::create(c, "imaginarium", "Imaginarium", "t")?.id;
            assert!(send(c, "vinculum", &["nope".into()], "x", "y", "info", None).is_err());
            assert!(send(c, "vinculum", &["antimony".into()], "Port?", "y", "gossip", None).is_err());

            let (t, _) = send(
                c,
                "vinculum",
                &["antimony".into()],
                "Port 21300 for the demo?",
                "Can I take 21300?",
                "resource",
                None,
            )?;
            let unread_a = crate::waiting::unread(c, a, "antimony")?;
            assert!(unread_a.iter().any(|e| e.summary.contains("Can I take 21300?")));
            assert!(crate::waiting::unread(c, i, "imaginarium")?.is_empty(), "not a member");
            assert!(crate::waiting::unread(c, v, "vinculum")?.is_empty(), "not its own");
            // Imaginarium isn't in it and can't write to it.
            assert!(send(c, "imaginarium", &[], "", "me too", "info", Some(t)).is_err());
            send(c, "antimony", &[], "", "Yes, take it.", "answer", Some(t))?;
            assert!(
                send(c, "antimony", &["imaginarium".into()], "", "fyi", "info", Some(t)).is_err(),
                "not the starter"
            );
            send(c, "antimony", &["vinculum".into()], "", "already in", "info", Some(t))?;
            assert_eq!(messages(c, t)?.len(), 3);

            // A message to everyone reaches every project but the sender; the consultant reads it.
            let token = crate::util::new_secret("af_prj_");
            send(
                c,
                "imaginarium",
                &[ALL.into()],
                "Release build tonight",
                &format!("22:00, 4 cores. {token}"),
                "resource",
                None,
            )?;
            assert!(crate::waiting::unread(c, v, "vinculum")?.iter().any(|e| e.summary.contains("22:00")));
            let inbox = consultant_inbox(c)?;
            assert_eq!(inbox.len(), 1);
            assert!(inbox[0].body.contains("[redacted]") && !inbox[0].body.contains("af_prj_"), "tokens never stored");
            assert_eq!(consultant_inbox(c)?.len(), 1, "still there until it's marked read");
            mark_consultant_read(c, inbox[0].id)?;
            assert!(consultant_inbox(c)?.is_empty(), "read");
            assert_eq!(threads(c, Some("vinculum"), 10, None)?.len(), 2, "its own and the one to all");
            Ok(())
        })
        .unwrap();
    }
}
