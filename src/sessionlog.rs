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

use rusqlite::{Connection, OptionalExtension, params};
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
    /// For a subagent: the model its replies came from (D-228).
    #[serde(default)]
    pub model: Option<String>,
    /// For a subagent: its `flow note <file>` calls, (when, the file's name), to tag the notes (D-229).
    #[serde(default)]
    pub note_calls: Vec<(i64, String)>,
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
        self.msgs.is_empty()
            && self.title.is_none()
            && self.addresses.is_empty()
            && self.subagent.is_none()
            && self.model.is_none()
            && self.note_calls.is_empty()
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

/// The file a `flow note <file> …` (or `agentflow note`) command uploads: its name only.
fn note_file(command: &str) -> Option<String> {
    let words: Vec<&str> = command.split_whitespace().collect();
    let at =
        words.windows(2).position(|w| (w[0].ends_with("flow") || w[0].ends_with("agentflow")) && w[1] == "note")?;
    // The first word that isn't an option or an option's value (every option of `note` takes one).
    let mut rest = words[at + 2..].iter();
    let file = loop {
        let w = rest.next()?;
        if w.starts_with("--") {
            if !w.contains('=') {
                rest.next();
            }
            continue;
        }
        break w;
    };
    let file = file.trim_matches(|c| c == '"' || c == '\'');
    let name = Path::new(file).file_name()?.to_str()?;
    (!name.is_empty()).then(|| name.chars().take(200).collect())
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
    let sub_model = sub.is_some() && found.model.is_none() && contains(buf, b"\"model\":\"");
    let sub_note = sub.is_some() && contains(buf, b" note ") && contains(buf, b"\"Bash\"");
    if !(title || cross || calls || sub_user || sub_end || sub_model || sub_note) {
        return;
    }
    let Ok(line) = serde_json::from_slice::<Value>(buf) else { return };
    let kind = line.get("type").and_then(Value::as_str).unwrap_or("");
    let at = line.get("timestamp").and_then(Value::as_str).and_then(parse_time);
    if kind == "custom-title" {
        if let Some(t) = line.get("customTitle").and_then(Value::as_str).map(str::trim).filter(|t| is_name(t)) {
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
        // The model a subagent runs on (a synthetic "<synthetic>" reply isn't one).
        if sub.is_some()
            && found.model.is_none()
            && let Some(m) = message.and_then(|m| m.get("model")).and_then(Value::as_str)
            && m.starts_with("claude")
        {
            found.model = Some(m.chars().take(60).collect());
        }
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
                // A subagent hands in a file as a note: `flow note <file> …` (D-229).
                Some("Bash") if sub.is_some() => {
                    if let Some(file) = note_file(&s("command")) {
                        found.note_calls.push((at, file));
                    }
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
    if kind == "user"
        && sub.is_some()
        && !line.get("isMeta").and_then(Value::as_bool).unwrap_or(false)
        && let Some(text) = message.and_then(|m| m.get("content")).and_then(text_of)
        && !text.trim().is_empty()
        && !text.trim_start().starts_with("<system-reminder>")
    {
        found.msgs.push(Msg { at, from: Who::Parent, to: Who::Me, body: clip(&text) });
    }
}

/// A title that can name a session: one with a letter or a digit. A stray `/rename \` gave
/// Imaginarium's session the name "\" (the owner, 2026-11-18: "who is '/'?"); such titles are
/// left out, and the name before stays.
/// Tags the notes subagents uploaded (D-229): a note of the same project, with the same file name,
/// made within ten minutes after a subagent's `flow note` call, gets `data.subagent` (its id, type,
/// description and model). Called when a call comes in and when a note does; either may come first.
pub fn tag_subagent_notes(conn: &Connection) -> anyhow::Result<usize> {
    let calls: Vec<(String, i64, String, Option<i64>)> = conn
        .prepare("SELECT agent_id, at, filename, project_id FROM subagent_note_calls WHERE done = 0")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<Result<_, _>>()?;
    let mut n = 0;
    for (agent, at, file, project) in calls {
        let note: Option<i64> = conn
            .query_row(
                "SELECT id FROM records WHERE kind = 'note' AND json_extract(data, '$.filename') = ?1
                   AND json_extract(data, '$.subagent') IS NULL AND created_at BETWEEN ?2 - 60000 AND ?2 + 600000
                   AND (?3 IS NULL OR project_id = ?3)
                 ORDER BY abs(created_at - ?2) LIMIT 1",
                params![file, at, project],
                |r| r.get(0),
            )
            .optional()?;
        let Some(note) = note else {
            // Given up on after a day: the upload failed or went elsewhere.
            if crate::util::now_ms() - at > 86_400_000 {
                conn.execute(
                    "UPDATE subagent_note_calls SET done = 1 WHERE agent_id = ?1 AND at = ?2 AND filename = ?3",
                    params![agent, at, file],
                )?;
            }
            continue;
        };
        let who: Option<(String, String, String)> = conn
            .query_row(
                "SELECT agent_type, description, model FROM subagent_threads WHERE agent_id = ?1",
                [&agent],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let (kind, description, model) = who.unwrap_or_default();
        let tag = serde_json::json!({"id": agent, "type": kind, "description": description, "model": model});
        conn.execute(
            "UPDATE records SET data = json_set(data, '$.subagent', json(?1)) WHERE id = ?2",
            params![tag.to_string(), note],
        )?;
        conn.execute(
            "UPDATE subagent_note_calls SET done = 1 WHERE agent_id = ?1 AND at = ?2 AND filename = ?3",
            params![agent, at, file],
        )?;
        n += 1;
    }
    Ok(n)
}

/// Fills in the model of subagent runs from before it was read (D-228: "can we do this
/// retroactively as well?"): from the usage records of its session in the run's time, only where
/// that's exactly one model.
pub fn backfill_models(conn: &Connection) -> anyhow::Result<usize> {
    let runs: Vec<(String, String, i64, i64)> = conn
        .prepare("SELECT agent_id, session, started_at, last_at FROM subagent_threads WHERE model = ''")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<Result<_, _>>()?;
    let mut n = 0;
    for (agent, session, from, to) in runs {
        let models: Vec<String> = conn
            .prepare(
                "SELECT DISTINCT model FROM usage WHERE session = ?1 AND subagent = 1 AND at BETWEEN ?2 - 60000 AND ?3 + 60000
                   AND model LIKE 'claude%'",
            )?
            .query_map(params![session, from, to], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        if let [m] = models.as_slice() {
            n += conn.execute("UPDATE subagent_threads SET model = ?1 WHERE agent_id = ?2", params![m, agent])?;
        }
    }
    Ok(n)
}

/// A model as people say it: "claude-fable-5-2" is "Fable 5.2", "claude-sonnet-4-5-20250929" is
/// "Sonnet 4.5"; "" stays "".
pub fn model_label(id: &str) -> String {
    let rest = id.strip_prefix("claude-").unwrap_or(id);
    let mut parts = rest.split('-').filter(|p| !(p.len() == 8 && p.bytes().all(|b| b.is_ascii_digit())));
    let Some(family) = parts.next().filter(|f| !f.is_empty()) else { return String::new() };
    let mut name = family[..1].to_uppercase() + &family[1..];
    let version: Vec<&str> = parts.take_while(|p| p.bytes().all(|b| b.is_ascii_digit())).collect();
    if !version.is_empty() {
        name.push(' ');
        name.push_str(&version.join("."));
    }
    name
}

/// A session's model, from its newest call of its own (not a subagent's).
pub fn session_model(conn: &Connection, session: &str) -> anyhow::Result<String> {
    Ok(conn
        .query_row(
            "SELECT model FROM usage WHERE session = ?1 AND subagent = 0 AND model LIKE 'claude%' ORDER BY at DESC LIMIT 1",
            [session],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_default())
}

pub fn is_name(t: &str) -> bool {
    t.chars().any(char::is_alphanumeric)
}

/// Repairs sessions named by such a title before it was left out: each gets back the name its own
/// transcript used most (else its project's or its folder's), and its conversations join that
/// name's (a message seen from both sides is kept once). Runs at start; does nothing once clean.
pub fn repair_names(conn: &Connection) -> anyhow::Result<usize> {
    let bad: Vec<(String, String, String)> = {
        let mut st = conn.prepare("SELECT session, name, folder FROM session_names")?;
        let rows =
            st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
        rows.collect::<Result<Vec<_>, _>>()?.into_iter().filter(|(_, n, _)| !is_name(n)).collect()
    };
    let mut fixed = 0;
    for (session, bad_name, folder) in bad {
        // The name in most of this session's own messages, from before the stray title.
        let mut st = conn.prepare(
            "SELECT name, count(*) AS n FROM (SELECT from_name AS name FROM session_messages WHERE session = ?1
               UNION ALL SELECT to_name FROM session_messages WHERE session = ?1)
             WHERE name != ?2 GROUP BY name ORDER BY n DESC",
        )?;
        let names: Vec<String> = st
            .query_map(params![session, bad_name], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|n| is_name(n))
            .collect();
        let project: Option<String> = conn
            .query_row(
                "SELECT p.name FROM session_messages m JOIN projects p ON p.id = m.project_id WHERE m.session = ?1 LIMIT 1",
                [&session],
                |r| r.get(0),
            )
            .ok();
        // Its own name among them: the one its project has, else the most used that no other
        // session goes by (the others are the sessions it talked to).
        let others: Vec<String> = {
            let mut st = conn.prepare("SELECT name FROM session_names WHERE session != ?1")?;
            let rows = st.query_map([&session], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            rows.iter().map(|n| norm(n)).collect()
        };
        let own = project
            .as_ref()
            .and_then(|p| names.iter().find(|n| norm(n) == norm(p)))
            .or_else(|| names.iter().find(|n| !others.contains(&norm(n))));
        let good =
            own.cloned().or(project).unwrap_or_else(|| folder.rsplit('-').next().unwrap_or("session").to_owned());
        conn.execute("UPDATE session_names SET name = ?1 WHERE session = ?2", params![good, session])?;
        // Its messages under the stray name: renamed, re-keyed, and moved to the right pair.
        let mut st = conn.prepare(
            "SELECT id, thread, from_name, to_name, body FROM session_messages WHERE from_name = ?1 OR to_name = ?1",
        )?;
        let rows: Vec<(i64, String, String, String, String)> = st
            .query_map([&bad_name], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
            .collect::<Result<_, _>>()?;
        for (id, thread, from, to, body) in rows {
            let from = if from == bad_name { good.clone() } else { from };
            let to = if to == bad_name { good.clone() } else { to };
            let thread = if thread.starts_with("pair:") {
                let mut pair = [norm(&from), norm(&to)];
                pair.sort();
                format!("pair:{}|{}", pair[0], pair[1])
            } else {
                thread
            };
            let k = key(&[&thread, &norm(&from), body.trim()]);
            // Seen from the other side already: keep that one.
            let dup: bool = conn.query_row(
                "SELECT count(*) FROM session_messages WHERE key = ?1 AND id != ?2",
                params![k, id],
                |r| r.get::<_, i64>(0),
            )? > 0;
            if dup {
                conn.execute("DELETE FROM session_messages WHERE id = ?1", [id])?;
            } else {
                conn.execute(
                    "UPDATE session_messages SET key = ?1, thread = ?2, from_name = ?3, to_name = ?4 WHERE id = ?5",
                    params![k, thread, from, to, id],
                )?;
            }
        }
        conn.execute("UPDATE session_addresses SET name = ?1 WHERE name = ?2", params![good, bad_name])?;
        fixed += 1;
    }
    Ok(fixed)
}

/// A subagent's id as SendMessage names it: "a" and 16 hex digits.
fn is_agent_id(s: &str) -> bool {
    s.len() >= 9 && s.starts_with('a') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// A session's name for grouping: "agent-flow-22", "somedesk-ac" and "agent-flow" are the same
/// session's (Claude Code gives untitled sessions their folder's name and two hex digits), case aside.
pub fn norm(name: &str) -> String {
    let n = name.trim().to_lowercase();
    match n.rsplit_once('-') {
        Some((base, tail)) if !base.is_empty() && tail.len() == 2 && tail.bytes().all(|b| b.is_ascii_hexdigit()) => {
            base.to_owned()
        }
        _ => n,
    }
}

/// A session's name as shown: without the two hex digits Claude Code adds to untitled sessions.
pub fn shown(name: &str) -> String {
    let n = name.trim();
    match n.rsplit_once('-') {
        Some((base, tail)) if !base.is_empty() && tail.len() == 2 && tail.bytes().all(|b| b.is_ascii_hexdigit()) => {
            base.to_owned()
        }
        _ => n.to_owned(),
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
        && is_name(t)
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
        if let Some(m) = &found.model {
            conn.execute(
                "UPDATE subagent_threads SET model = ?1 WHERE agent_id = ?2 AND model = ''",
                params![m, s.id],
            )?;
        }
        for (at, file) in &found.note_calls {
            conn.execute(
                "INSERT OR IGNORE INTO subagent_note_calls (agent_id, at, filename, project_id) VALUES (?1, ?2, ?3, ?4)",
                params![s.id, at, file, project_id],
            )?;
        }
        if !found.note_calls.is_empty() {
            tag_subagent_notes(conn)?;
        }
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
        let added = conn.execute(
            "INSERT OR IGNORE INTO session_messages (key, at, thread, from_name, to_name, body, folder, cwd, session, project_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![k, m.at, thread, from, to, m.body, folder, cwd, own_session, project_id],
        )?;
        n += added;
        // In the Log too, under Conversations (the owner, 2026-11-17), once.
        if added == 1 {
            let project: Option<String> = project_id
                .and_then(|id| conn.query_row("SELECT slug FROM projects WHERE id = ?1", [id], |r| r.get(0)).ok());
            let kind = if thread.starts_with("sub:") { "subagent" } else { "message" };
            conn.execute(
                "INSERT INTO log (at, actor_kind, actor, project, via, action, summary, target, severity)
                 VALUES (?1, 'session', ?2, ?3, 'session', ?4, ?5, ?6, 'info')",
                params![
                    m.at,
                    crate::activity::clip(&from, 80),
                    project,
                    kind,
                    crate::activity::clip(&format!("{from} → {to}: {}", m.body.trim()), 300),
                    thread
                ],
            )?;
        }
        if let Some(id) = thread.strip_prefix("sub:") {
            conn.execute(
                "UPDATE subagent_threads SET last_at = max(last_at, ?1) WHERE agent_id = ?2",
                params![m.at, id],
            )?;
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
    /// Who takes part, as shown: a pair's two sessions (the one that wrote first first), or a
    /// subagent's session.
    pub parties: Vec<String>,
    /// A subagent's type ("fork", "general-purpose"), or "".
    pub agent_type: String,
    /// The models of its parties, as raw ids ("claude-fable-5-2"), "" where unknown (D-228): for a
    /// subagent its session's and its own, for a pair each session's.
    pub models: Vec<String>,
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
        let (mut na, mut nb) = (shown(a), shown(b));
        // The side that wrote first comes first: the row names the initiator first and tags only
        // its model (the owner, 2026-11-19: "maybe it would be better if we only mention the initiator").
        let first: Option<String> = conn
            .query_row("SELECT from_name FROM session_messages WHERE thread = ?1 ORDER BY at LIMIT 1", [&key], |r| {
                r.get(0)
            })
            .optional()?;
        if first.is_some_and(|f| norm(&f) == norm(&nb) && norm(&f) != norm(&na)) {
            std::mem::swap(&mut na, &mut nb);
        }
        // Each side's model: the newest of the session with that name (D-228).
        let model_of = |name: &str| -> anyhow::Result<String> {
            let session: Option<String> = conn
                .query_row("SELECT session FROM session_names WHERE name = ?1 ORDER BY at DESC LIMIT 1", [name], |r| {
                    r.get(0)
                })
                .optional()?;
            Ok(match session {
                Some(s) => session_model(conn, &s)?,
                None => String::new(),
            })
        };
        let models = vec![model_of(&na)?, model_of(&nb)?];
        let parties = vec![self::shown(&na), self::shown(&nb)];
        out.push(Thread {
            models,
            title: format!("{} ↔ {}", parties[0], parties[1]),
            key,
            sub: String::new(),
            project_id,
            count,
            last_at,
            is_subagent: false,
            parties,
            agent_type: String::new(),
        });
    }
    Ok(out)
}

/// Subagents with what they were told and reported, newest first; of one project when given.
pub fn subagent_threads(conn: &Connection, project_id: Option<i64>, limit: i64) -> anyhow::Result<Vec<Thread>> {
    let mut stmt = conn.prepare(
        "SELECT s.agent_id, s.description, s.agent_type, s.session, s.folder, s.cwd, s.project_id, s.last_at,
                (SELECT count(*) FROM session_messages m WHERE m.thread = 'sub:' || s.agent_id), s.model
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
            r.get::<_, String>(9)?,
        ))
    })?;
    let mut out = Vec::new();
    let mut session_models: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for row in rows {
        let (id, description, agent_type, session, folder, cwd, project_id, last_at, count, model) = row?;
        let parent_model = match session_models.get(&session) {
            Some(m) => m.clone(),
            None => {
                let m = session_model(conn, &session)?;
                session_models.insert(session.clone(), m.clone());
                m
            }
        };
        let session_name: String = conn
            .query_row("SELECT name FROM session_names WHERE session = ?1", [&session], |r| r.get(0))
            .unwrap_or_else(|_| {
                Path::new(&cwd).file_name().and_then(|f| f.to_str()).map(str::to_owned).unwrap_or(folder)
            });
        out.push(Thread {
            key: format!("sub:{id}"),
            title: if description.is_empty() { "Subagent".into() } else { description },
            parties: vec![self::shown(&session_name)],
            models: vec![parent_model, model],
            agent_type: agent_type.clone(),
            sub: [agent_type, format!("for {session_name}")]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" · "),
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

/// The threads with a message that matches every term, and when the newest such message was: a
/// term is a word in the text, or `from:name` / `to:name` (any case, part of the name).
pub fn search(conn: &Connection, terms: &[(String, String)]) -> anyhow::Result<std::collections::HashMap<String, i64>> {
    let mut sql = String::from("SELECT thread, max(at) FROM session_messages WHERE 1");
    let mut args: Vec<String> = Vec::new();
    for (k, v) in terms {
        let col = match k.as_str() {
            "from" => "from_name",
            "to" => "to_name",
            _ => "body",
        };
        let like = format!("%{}%", v.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        args.push(like);
        sql += &format!(" AND {col} LIKE ?{} ESCAPE '\\'", args.len());
    }
    sql += " GROUP BY thread";
    let mut stmt = conn.prepare(&sql)?;
    let rows =
        stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn messages(conn: &Connection, thread: &str) -> anyhow::Result<Vec<LogMsg>> {
    let mut stmt =
        conn.prepare("SELECT at, from_name, to_name, body FROM session_messages WHERE thread = ?1 ORDER BY at, id")?;
    let rows =
        stmt.query_map([thread], |r| Ok(LogMsg { at: r.get(0)?, from: r.get(1)?, to: r.get(2)?, body: r.get(3)? }))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_tags_and_lines() {
        assert_eq!(norm("agent-flow-22"), "agent-flow");
        assert_eq!(norm("Viber Consultant"), "viber consultant");
        assert_eq!(norm("somedesk-ac"), "somedesk");
        assert_eq!(norm("realer-xpad-5d"), "realer-xpad");
        assert_eq!(norm("antimony-mcp"), "antimony-mcp");
        let t = "Another Claude session sent a message:\n<cross-session-message from=\"uds:/run/x.sock\" from-name=\"Imaginarium\" from-mode=\"prompting\">\nHello there\n</cross-session-message>";
        assert_eq!(cross_session(t), vec![("uds:/run/x.sock".into(), "Imaginarium".into(), "Hello there".into())]);
        assert!(is_agent_id("a0ca2a7e879b8b08a") && !is_agent_id("Imaginarium") && !is_agent_id("all"));

        let mut f = Found::default();
        let line =
            serde_json::json!({"type": "queue-operation", "operation": "enqueue", "timestamp": "2026-11-12T10:00:00Z",
            "content": t})
            .to_string();
        scan(line.as_bytes(), None, &mut f);
        let line =
            serde_json::json!({"type": "assistant", "timestamp": "2026-11-12T10:01:00Z", "message": {"content": [
            {"type": "tool_use", "name": "SendMessage", "input": {"to": "uds:/run/x.sock", "message": "Hi back"}}]}})
            .to_string();
        scan(line.as_bytes(), None, &mut f);
        let line = serde_json::json!({"type": "custom-title", "customTitle": "agent-flow"}).to_string();
        scan(line.as_bytes(), None, &mut f);
        assert_eq!(f.msgs.len(), 2);
        assert_eq!(f.msgs[1].to, Who::Address("uds:/run/x.sock".into()));
        assert_eq!(f.title.as_deref(), Some("agent-flow"));

        let sub = SubagentInfo {
            id: "a1".into(),
            parent_session: "s".into(),
            description: "Review".into(),
            agent_type: "general".into(),
        };
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

    #[test]
    fn a_stray_title_is_left_out_and_repaired() {
        assert!(!is_name("\\") && !is_name(" / ") && is_name("Imaginarium") && is_name("v2"));
        let db = crate::db::Db::open_memory();
        db.call_blocking(|c| {
            let ins = |k: &str, thread: &str, from: &str, to: &str, body: &str, session: &str| {
                c.execute(
                    "INSERT INTO session_messages (key, at, thread, from_name, to_name, body, session) VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6)",
                    params![k, thread, from, to, body, session],
                )
                .unwrap();
            };
            // Imaginarium's own transcript, before and after the stray title; agent-flow's has one of them too.
            ins(&key(&["pair:agent-flow|imaginarium", "imaginarium", "hello"]), "pair:agent-flow|imaginarium", "Imaginarium", "agent-flow", "hello", "s2");
            ins(&key(&["pair:agent-flow|imaginarium", "agent-flow", "hi"]), "pair:agent-flow|imaginarium", "agent-flow", "Imaginarium", "hi", "s2");
            ins("k3", "pair:\\|agent-flow", "\\", "agent-flow", "later", "s2");
            ins("k4", "pair:\\|agent-flow", "agent-flow", "\\", "seen twice", "s2");
            ins(&key(&["pair:agent-flow|imaginarium", "agent-flow", "seen twice"]), "pair:agent-flow|imaginarium", "agent-flow", "Imaginarium", "seen twice", "s1");
            c.execute("INSERT INTO session_names (session, name, folder, at) VALUES ('s2', '\\', '-home-viber-office', 1)", []).unwrap();
            c.execute(
                "INSERT INTO session_names (session, name, folder, at) VALUES ('s1', 'agent-flow', '-home-viber-agent-flow', 1)",
                [],
            )
            .unwrap();
            assert_eq!(repair_names(c).unwrap(), 1);
            let n: String = c.query_row("SELECT name FROM session_names WHERE session = 's2'", [], |r| r.get(0)).unwrap();
            assert_eq!(n, "Imaginarium");
            let left: i64 = c.query_row("SELECT count(*) FROM session_messages WHERE thread != 'pair:agent-flow|imaginarium'", [], |r| r.get(0)).unwrap();
            assert_eq!(left, 0, "one pair");
            let all: i64 = c.query_row("SELECT count(*) FROM session_messages", [], |r| r.get(0)).unwrap();
            assert_eq!(all, 4, "the message seen from both sides is kept once");
            assert_eq!(repair_names(c).unwrap(), 0, "clean now");
            Ok(())
        })
        .unwrap();
    }

    /// D-228, D-229: a subagent's model and its `flow note` calls from its transcript; old runs'
    /// models from the usage records, only where that's one model.
    #[test]
    fn models_and_note_calls() {
        assert_eq!(model_label("claude-fable-5-2"), "Fable 5.2");
        assert_eq!(model_label("claude-sonnet-4-5-20250929"), "Sonnet 4.5");
        assert_eq!(model_label("claude-haiku-5-5"), "Haiku 5.5");
        assert_eq!(model_label(""), "");
        assert_eq!(
            note_file("cd /x && flow note /tmp/s/handover.md --name \"H\" --category handover").as_deref(),
            Some("handover.md")
        );
        assert_eq!(note_file("agentflow note --name 'Report' --category=x ./report.md").as_deref(), Some("report.md"));
        assert_eq!(note_file("flow remote -- cargo test"), None);
        let sub = SubagentInfo {
            id: "a1".into(),
            parent_session: "s1".into(),
            description: "Handover".into(),
            agent_type: "code-review".into(),
        };
        let mut found = Found::default();
        let line = serde_json::json!({"type": "assistant", "timestamp": "2026-11-19T10:00:00Z", "message": {"model": "claude-fable-5-2", "content": [
            {"type": "tool_use", "name": "Bash", "input": {"command": "flow note /tmp/s/handover.md --name Handover --category handover"}}]}});
        scan(line.to_string().as_bytes(), Some(&sub), &mut found);
        assert_eq!(found.model.as_deref(), Some("claude-fable-5-2"));
        assert_eq!(found.note_calls.len(), 1);
        assert_eq!(found.note_calls[0].1, "handover.md");

        let db = crate::db::Db::open_memory();
        db.call_blocking(|c| {
            for (id, from, to) in [("r1", 1_000_000, 1_300_000), ("r2", 3_000_000, 3_600_000)] {
                c.execute(
                    "INSERT INTO subagent_threads (agent_id, session, folder, started_at, last_at) VALUES (?1, 's1', 'f', ?2, ?3)",
                    params![id, from, to],
                )?;
            }
            let call = |id: &str, at: i64, model: &str, sub: bool| {
                c.execute(
                    "INSERT INTO usage (message_id, at, folder, session, subagent, model, input, output, cache_write, cache_read)
                     VALUES (?1, ?2, 'f', 's1', ?3, ?4, 1, 1, 0, 0)",
                    params![id, at, sub, model],
                )
                .unwrap();
            };
            call("m1", 1_100_000, "claude-fable-5-2", true);
            call("m2", 3_100_000, "claude-fable-5-2", true);
            call("m3", 3_400_000, "claude-haiku-5-5", true);
            call("m4", 3_500_000, "claude-opus-5-5", false);
            assert_eq!(backfill_models(c)?, 1, "r2 had two models in its time: left unknown");
            let m: String = c.query_row("SELECT model FROM subagent_threads WHERE agent_id = 'r1'", [], |r| r.get(0))?;
            assert_eq!(m, "claude-fable-5-2");
            assert_eq!(session_model(c, "s1")?, "claude-opus-5-5", "the session's own calls, not its subagents'");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_pair_that_wrote_first_comes_first() {
        let db = crate::db::Db::open_memory();
        db.call_blocking(|c| {
            for (k, at, from, to) in [("k1", 10, "zeta", "alpha"), ("k2", 20, "alpha", "zeta")] {
                c.execute(
                    "INSERT INTO session_messages (key, at, thread, from_name, to_name, body) VALUES (?1, ?2, 'pair:alpha|zeta', ?3, ?4, 'hi')",
                    params![k, at, from, to],
                )?;
            }
            let t = session_threads(c, 10)?;
            assert_eq!(t[0].parties, ["zeta", "alpha"], "zeta started it, though the key sorts alpha first");
            Ok(())
        })
        .unwrap();
    }
}
