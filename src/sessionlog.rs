//! The session log: what Claude Code sessions and their subagents tell each other, read from the
//! transcripts (the owner, 2026-11-12: "keep the claude code sendmessage and use agent flow only
//! for logging for this", and "log all inter-agent (between projects) conversations as well as
//! intraproject (subagents) conversations"). AgentFlow only reads; agents do nothing for it.
//!
//! What a transcript holds:
//! - a message another session sent: `<cross-session-message from="…" from-name="…">body</…>` in a
//!   queued or user line of the receiving session;
//! - a message this session sent: a `SendMessage` call (`to`, `message`);
//! - a subagent's brief: the first user line of its own transcript (`subagents/agent-<id>.jsonl`,
//!   with `agent-<id>.meta.json` beside it); later messages from its parent are user lines too;
//! - a subagent's report: its `SubagentHandback` call, or the text it ends its turn with.
//!
//! A message seen from both sides (sent here, received there) is kept once: its key is the
//! sender and the text.

use std::path::Path;

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The longest message kept; the rest is cut (a brief can be long, a whole report rarely is).
const MAX_BODY: usize = 64 * 1024;

/// Who a message is from or to, as the transcript says it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "t", content = "v", rename_all = "snake_case")]
pub enum Who {
    /// The session whose transcript this is (named at store time).
    Me,
    /// A session by its name ("Viber Consultant", "agent-flow-22").
    Named(String),
    /// A session by its address (`uds:…`, `bridge:…`), named once it's known.
    Address(String),
    /// A subagent, by its id.
    Subagent(String),
    /// The parent of the subagent whose transcript this is.
    Parent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Msg {
    pub at: i64,
    pub from: Who,
    pub to: Who,
    pub body: String,
}

/// What a transcript's new lines held for the log.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Found {
    pub msgs: Vec<Msg>,
    /// The session's own name, when it has one (`/rename`).
    pub title: Option<String>,
    /// Addresses learned from received messages: (address, name).
    pub addresses: Vec<(String, String)>,
    /// For a subagent's transcript: its id, its parent session, and its description and type.
    pub subagent: Option<SubagentInfo>,
    pub first_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentInfo {
    pub id: String,
    pub parent_session: String,
    pub description: String,
    pub agent_type: String,
}

impl Found {
    pub fn is_empty(&self) -> bool {
        self.msgs.is_empty() && self.title.is_none() && self.addresses.is_empty() && self.subagent.is_none()
    }
}

/// A subagent's id and parent session from its transcript's path
/// (`<folder>/<session>/subagents/agent-<id>.jsonl`), with what its `.meta.json` says.
pub fn subagent_of(path: &Path) -> Option<SubagentInfo> {
    let name = path.file_stem()?.to_str()?;
    let id = name.strip_prefix("agent-")?.to_owned();
    let dir = path.parent()?;
    if dir.file_name()? != "subagents" {
        return None;
    }
    let parent_session = dir.parent()?.file_name()?.to_str()?.to_owned();
    let meta: Value = std::fs::read(path.with_file_name(format!("{name}.meta.json")))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let s = |k: &str| meta.get(k).and_then(Value::as_str).unwrap_or("").chars().take(200).collect::<String>();
    Some(SubagentInfo { id, parent_session, description: s("description"), agent_type: s("agentType") })
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn clip(s: &str) -> String {
    let s = s.trim();
    if s.len() <= MAX_BODY {
        return s.to_owned();
    }
    let mut end = MAX_BODY;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n\n[… cut: {} KB in all]", &s[..end], s.len() / 1024)
}

/// The text of a line's message: a string, or its text blocks (not tool results).
fn text_of(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let texts: Vec<&str> = parts
                .iter()
                .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect();
            let other = parts.iter().any(|p| p.get("type").and_then(Value::as_str) != Some("text"));
            (!texts.is_empty() && !other).then(|| texts.join("\n\n"))
        }
        _ => None,
    }
}

/// The cross-session messages in a text: (from address, from name, body).
fn cross_session(text: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("<cross-session-message") {
        let tag_rest = &rest[i..];
        let Some(close) = tag_rest.find('>') else { break };
        let tag = &tag_rest[..close];
        let attr = |name: &str| -> String {
            let pat = format!(" {name}=\"");
            tag.find(&pat)
                .and_then(|j| {
                    let v = &tag[j + pat.len()..];
                    v.find('"').map(|k| v[..k].to_owned())
                })
                .unwrap_or_default()
        };
        let body_rest = &tag_rest[close + 1..];
        let Some(end) = body_rest.find("</cross-session-message>") else { break };
        out.push((attr("from"), attr("from-name"), body_rest[..end].trim().to_owned()));
        rest = &body_rest[end..];
    }
    out
}

fn parse_time(s: &str) -> Option<i64> {
    let t = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()?;
    Some((t.unix_timestamp_nanos() / 1_000_000) as i64)
}

/// Looks at one transcript line for the log. Cheap checks first: most lines are none of these.
pub fn scan(buf: &[u8], sub: Option<&SubagentInfo>, found: &mut Found) {
    let title = contains(buf, b"\"custom-title\"");
    let cross = contains(buf, b"<cross-session-message");
    let calls = contains(buf, b"\"SendMessage\"") || contains(buf, b"\"SubagentHandback\"");
    let sub_user = sub.is_some() && contains(buf, b"\"type\":\"user\"");
    let sub_end = sub.is_some() && contains(buf, b"\"end_turn\"");
    if !(title || cross || calls || sub_user || sub_end) {
        return;
    }
    let Ok(line) = serde_json::from_slice::<Value>(buf) else { return };
    let kind = line.get("type").and_then(Value::as_str).unwrap_or("");
    let at = line.get("timestamp").and_then(Value::as_str).and_then(parse_time);
    if kind == "custom-title" {
        if let Some(t) = line.get("customTitle").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()) {
            found.title = Some(t.chars().take(100).collect());
        }
        return;
    }
    let Some(at) = at else { return };
    if found.first_at.is_none() {
        found.first_at = Some(at);
    }
    // Received from another session: queued, or shown as a user line (kept once by its key).
    if cross && (kind == "queue-operation" || kind == "user") {
        let text = match kind {
            "queue-operation" => line.get("content").and_then(Value::as_str).map(str::to_owned),
            _ => line.get("message").and_then(|m| m.get("content")).and_then(text_of),
        };
        for (addr, name, body) in cross_session(text.as_deref().unwrap_or("")) {
            if body.is_empty() {
                continue;
            }
            let from = if !name.is_empty() { Who::Named(name.clone()) } else { Who::Address(addr.clone()) };
            if !addr.is_empty() && !name.is_empty() {
                found.addresses.push((addr, name));
            }
            found.msgs.push(Msg { at, from, to: Who::Me, body: clip(&body) });
        }
        return;
    }
    let message = line.get("message");
    if kind == "assistant" {
        let content = message.and_then(|m| m.get("content"));
        for part in content.and_then(Value::as_array).into_iter().flatten() {
            if part.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let input = part.get("input");
            let s = |k: &str| input.and_then(|i| i.get(k)).and_then(Value::as_str).unwrap_or("").trim().to_owned();
            match part.get("name").and_then(Value::as_str) {
                Some("SendMessage") => {
                    let (to, body) = (s("to"), s("message"));
                    if to.is_empty() || body.is_empty() {
                        continue;
                    }
                    let to = if sub.is_some() && to == "main" {
                        Who::Parent
                    } else if to.starts_with("uds:") || to.starts_with("bridge:") {
                        Who::Address(to.split(" [").next().unwrap_or(&to).to_owned())
                    } else if is_agent_id(&to) {
                        Who::Subagent(to)
                    } else {
                        Who::Named(to.split(" [").next().unwrap_or(&to).to_owned())
                    };
                    found.msgs.push(Msg { at, from: Who::Me, to, body: clip(&body) });
                }
                Some("SubagentHandback") if sub.is_some() => {
                    let body = s("message");
                    if !body.is_empty() {
                        found.msgs.push(Msg { at, from: Who::Me, to: Who::Parent, body: clip(&body) });
                    }
                }
                _ => {}
            }
        }
        // A subagent's last words when it ends its turn with plain text.
        if sub.is_some()
            && message.and_then(|m| m.get("stop_reason")).and_then(Value::as_str) == Some("end_turn")
            && let Some(text) = content.and_then(text_of)
            && !text.trim().is_empty()
        {
            found.msgs.push(Msg { at, from: Who::Me, to: Who::Parent, body: clip(&text) });
        }
        return;
    }
    // A subagent's brief and what its parent sends it later: its user lines with text (not tool
    // results, not Claude Code's own reminders).
    if kind == "user" && sub.is_some() && !line.get("isMeta").and_then(Value::as_bool).unwrap_or(false)
        && let Some(text) = message.and_then(|m| m.get("content")).and_then(text_of)
        && !text.trim().is_empty()
        && !text.trim_start().starts_with("<system-reminder>")
    {
        found.msgs.push(Msg { at, from: Who::Parent, to: Who::Me, body: clip(&text) });
    }
}

/// A subagent's id as SendMessage names it: "a" and 16 hex digits.
fn is_agent_id(s: &str) -> bool {
    s.len() >= 9 && s.starts_with('a') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// A session's name for grouping: "agent-flow-22" and "agent-flow" are the same session's
/// (Claude Code numbers untitled sessions), case aside.
pub fn norm(name: &str) -> String {
    let n = name.trim().to_lowercase();
    match n.rsplit_once('-') {
        Some((base, num)) if !base.is_empty() && !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) => base.to_owned(),
        _ => n,
    }
}

fn key(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0]);
    }
    crate::util::hex(&h.finalize()[..16])
}

/// Stores what a transcript's new lines held. `session` and `folder` are the transcript's;
/// `cwd` its working directory; `project_id` the project it belongs to.
pub fn store(
    conn: &Connection,
    found: &Found,
    session: &str,
    folder: &str,
    cwd: &str,
    project_id: Option<i64>,
) -> anyhow::Result<usize> {
    let now = crate::util::now_ms();
    if let Some(t) = &found.title
        && !session.is_empty()
    {
        conn.execute(
            "INSERT INTO session_names (session, name, folder, at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (session) DO UPDATE SET name = excluded.name, at = excluded.at",
            params![session, t, folder, now],
        )?;
    }
    for (addr, name) in &found.addresses {
        conn.execute(
            "INSERT INTO session_addresses (address, name, at) VALUES (?1, ?2, ?3)
             ON CONFLICT (address) DO UPDATE SET name = excluded.name, at = excluded.at",
            params![addr, name, now],
        )?;
    }
    if let Some(s) = &found.subagent {
        let at = found.first_at.unwrap_or(now);
        conn.execute(
            "INSERT INTO subagent_threads (agent_id, session, folder, cwd, description, agent_type, project_id, started_at, last_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
             ON CONFLICT (agent_id) DO UPDATE SET
               description = CASE WHEN excluded.description != '' THEN excluded.description ELSE description END,
               agent_type = CASE WHEN excluded.agent_type != '' THEN excluded.agent_type ELSE agent_type END,
               project_id = coalesce(project_id, excluded.project_id), started_at = min(started_at, excluded.started_at)",
            params![s.id, s.parent_session, folder, cwd, s.description, s.agent_type, project_id, at],
        )?;
    }
    // This session's name: its own title, or (for a subagent) its parent's; else its folder's last
    // part, as Claude Code names untitled sessions.
    let own_session = found.subagent.as_ref().map(|s| s.parent_session.as_str()).unwrap_or(session);
    let me: String = conn
        .query_row("SELECT name FROM session_names WHERE session = ?1", [own_session], |r| r.get(0))
        .unwrap_or_else(|_| {
            Path::new(cwd).file_name().and_then(|f| f.to_str()).map(str::to_owned).unwrap_or_else(|| folder.to_owned())
        });
    let name_of = |w: &Who| -> String {
        match w {
            Who::Me => me.clone(),
            Who::Named(n) => n.clone(),
            Who::Address(a) => conn
                .query_row("SELECT name FROM session_addresses WHERE address = ?1", [a], |r| r.get(0))
                .unwrap_or_else(|_| a.clone()),
            Who::Subagent(id) => format!("subagent {id}"),
            Who::Parent => me.clone(),
        }
    };
    let mut n = 0;
    for m in &found.msgs {
        // In a subagent's transcript, "me" is the subagent and its parent is the session.
        let (thread, from, to) = match (&found.subagent, &m.to, &m.from) {
            (Some(s), _, _) => {
                let sub_name = if s.description.is_empty() { "subagent".to_owned() } else { s.description.clone() };
                let (f, t) = if m.from == Who::Parent { (me.clone(), sub_name) } else { (sub_name, me.clone()) };
                (format!("sub:{}", s.id), f, t)
            }
            (None, Who::Subagent(id), _) => (format!("sub:{id}"), me.clone(), "subagent".to_owned()),
            (None, _, _) => {
                let (f, t) = (name_of(&m.from), name_of(&m.to));
                let mut pair = [norm(&f), norm(&t)];
                pair.sort();
                (format!("pair:{}|{}", pair[0], pair[1]), f, t)
            }
        };
        let k = key(&[&thread, &norm(&from), m.body.trim()]);
        n += conn.execute(
            "INSERT OR IGNORE INTO session_messages (key, at, thread, from_name, to_name, body, folder, cwd, session, project_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![k, m.at, thread, from, to, m.body, folder, cwd, own_session, project_id],
        )?;
        if let Some(id) = thread.strip_prefix("sub:") {
            conn.execute("UPDATE subagent_threads SET last_at = max(last_at, ?1) WHERE agent_id = ?2", params![m.at, id])?;
        }
    }
    Ok(n)
}

/// A conversation on the page.
#[derive(Debug, Clone)]
pub struct Thread {
    pub key: String,
    /// "agent-flow ↔ Imaginarium", or a subagent's description.
    pub title: String,
    /// For a subagent: its type and its session's name.
    pub sub: String,
    pub project_id: Option<i64>,
    pub count: i64,
    pub last_at: i64,
    pub is_subagent: bool,
}

/// Conversations between sessions, newest first.
pub fn session_threads(conn: &Connection, limit: i64) -> anyhow::Result<Vec<Thread>> {
    let mut stmt = conn.prepare(
        "SELECT thread, count(*), max(at), max(project_id) FROM session_messages WHERE thread LIKE 'pair:%'
         GROUP BY thread ORDER BY max(at) DESC LIMIT ?1",
    )?;
    let rows: Vec<(String, i64, i64, Option<i64>)> =
        stmt.query_map([limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<_, _>>()?;
    let mut out = Vec::new();
    for (key, count, last_at, project_id) in rows {
        // The names as last written, one per side.
        let (a, b) = key.trim_start_matches("pair:").split_once('|').unwrap_or(("", ""));
        let shown = |n: &str| -> String {
            conn.query_row(
                "SELECT name FROM (SELECT from_name AS name, at FROM session_messages WHERE thread = ?1
                   UNION ALL SELECT to_name, at FROM session_messages WHERE thread = ?1)
                 ORDER BY at DESC",
                [&key],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .filter(|x| norm(x) == n)
            .or_else(|| {
                let mut st = conn
                    .prepare(
                        "SELECT from_name FROM session_messages WHERE thread = ?1 UNION SELECT to_name FROM session_messages WHERE thread = ?1",
                    )
                    .ok()?;
                let names: Vec<String> = st.query_map([&key], |r| r.get(0)).ok()?.filter_map(Result::ok).collect();
                names.into_iter().find(|x| norm(x) == n)
            })
            .unwrap_or_else(|| n.to_owned())
        };
        out.push(Thread {
            title: format!("{} ↔ {}", shown(a), shown(b)),
            key,
            sub: String::new(),
            project_id,
            count,
            last_at,
            is_subagent: false,
        });
    }
    Ok(out)
}

/// Subagents with what they were told and reported, newest first; of one project when given.
pub fn subagent_threads(conn: &Connection, project_id: Option<i64>, limit: i64) -> anyhow::Result<Vec<Thread>> {
    let mut stmt = conn.prepare(
        "SELECT s.agent_id, s.description, s.agent_type, s.session, s.folder, s.cwd, s.project_id, s.last_at,
                (SELECT count(*) FROM session_messages m WHERE m.thread = 'sub:' || s.agent_id)
         FROM subagent_threads s
         WHERE (?1 IS NULL OR s.project_id = ?1)
           AND EXISTS (SELECT 1 FROM session_messages m WHERE m.thread = 'sub:' || s.agent_id)
         ORDER BY s.last_at DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![project_id, limit], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, Option<i64>>(6)?,
            r.get::<_, i64>(7)?,
            r.get::<_, i64>(8)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, description, agent_type, session, folder, cwd, project_id, last_at, count) = row?;
        let session_name: String = conn
            .query_row("SELECT name FROM session_names WHERE session = ?1", [&session], |r| r.get(0))
            .unwrap_or_else(|_| Path::new(&cwd).file_name().and_then(|f| f.to_str()).map(str::to_owned).unwrap_or(folder));
        out.push(Thread {
            key: format!("sub:{id}"),
            title: if description.is_empty() { "Subagent".into() } else { description },
            sub: [agent_type, format!("for {session_name}")].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · "),
            project_id,
            count,
            last_at,
            is_subagent: true,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct LogMsg {
    pub at: i64,
    pub from: String,
    pub to: String,
    pub body: String,
}

pub fn messages(conn: &Connection, thread: &str) -> anyhow::Result<Vec<LogMsg>> {
    let mut stmt =
        conn.prepare("SELECT at, from_name, to_name, body FROM session_messages WHERE thread = ?1 ORDER BY at, id")?;
    let rows = stmt.query_map([thread], |r| Ok(LogMsg { at: r.get(0)?, from: r.get(1)?, to: r.get(2)?, body: r.get(3)? }))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_tags_and_lines() {
        assert_eq!(norm("agent-flow-22"), "agent-flow");
        assert_eq!(norm("Viber Consultant"), "viber consultant");
        assert_eq!(norm("M9-2026"), "m9");
        let t = "Another Claude session sent a message:\n<cross-session-message from=\"uds:/run/x.sock\" from-name=\"Imaginarium\" from-mode=\"prompting\">\nHello there\n</cross-session-message>";
        assert_eq!(cross_session(t), vec![("uds:/run/x.sock".into(), "Imaginarium".into(), "Hello there".into())]);
        assert!(is_agent_id("a0ca2a7e879b8b08a") && !is_agent_id("Imaginarium") && !is_agent_id("all"));

        let mut f = Found::default();
        let line = serde_json::json!({"type": "queue-operation", "operation": "enqueue", "timestamp": "2026-11-12T10:00:00Z",
            "content": t}).to_string();
        scan(line.as_bytes(), None, &mut f);
        let line = serde_json::json!({"type": "assistant", "timestamp": "2026-11-12T10:01:00Z", "message": {"content": [
            {"type": "tool_use", "name": "SendMessage", "input": {"to": "uds:/run/x.sock", "message": "Hi back"}}]}}).to_string();
        scan(line.as_bytes(), None, &mut f);
        let line = serde_json::json!({"type": "custom-title", "customTitle": "agent-flow"}).to_string();
        scan(line.as_bytes(), None, &mut f);
        assert_eq!(f.msgs.len(), 2);
        assert_eq!(f.msgs[1].to, Who::Address("uds:/run/x.sock".into()));
        assert_eq!(f.title.as_deref(), Some("agent-flow"));

        let sub = SubagentInfo { id: "a1".into(), parent_session: "s".into(), description: "Review".into(), agent_type: "general".into() };
        let mut f = Found::default();
        for line in [
            serde_json::json!({"type": "user", "parentUuid": null, "timestamp": "2026-11-12T10:00:00Z", "message": {"content": "Review the diff"}}),
            serde_json::json!({"type": "user", "isMeta": true, "timestamp": "2026-11-12T10:00:01Z", "message": {"content": "<system-reminder>x"}}),
            serde_json::json!({"type": "user", "timestamp": "2026-11-12T10:00:02Z", "message": {"content": [{"type": "tool_result", "content": "ok"}]}}),
            serde_json::json!({"type": "assistant", "timestamp": "2026-11-12T10:05:00Z", "message": {"stop_reason": "tool_use", "content": [
                {"type": "tool_use", "name": "SubagentHandback", "input": {"message": "Two findings"}}]}}),
        ] {
            scan(line.to_string().as_bytes(), Some(&sub), &mut f);
        }
        let bodies: Vec<&str> = f.msgs.iter().map(|m| m.body.as_str()).collect();
        assert_eq!(bodies, ["Review the diff", "Two findings"]);
        assert_eq!((&f.msgs[0].from, &f.msgs[1].to), (&Who::Parent, &Who::Parent));
    }
}
