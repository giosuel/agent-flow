//! Token usage from Claude Code's transcripts (`~/.claude/projects/**/*.jsonl`). The owner,
//! 2026-11-10: "It should be logged per project and there should be an overall usage histogram
//! (per day and hour on zoom) of token usage as well as one line per agent and a detailed overview
//! of all tasks and their token usage in a separate new tab."
//!
//! Each transcript is read from where the last round stopped. Every model call is one row, keyed
//! by its message id: a subagent's transcript repeats some of its parent's, and a forked session
//! copies them, so the id is what keeps a call from counting twice.
//!
//! A session works on one thing at a time, so its tokens go to the task its project's agent last
//! touched through AgentFlow; a subagent's go to the task its session was on when the subagent
//! started (the owner agreed, 2026-11-10). No even split.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read as _, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::util::expand_home;

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: String,
    timestamp: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    cwd: Option<String>,
    message: Option<Message>,
    /// A remote-control line: `{"type":"system","subtype":"bridge_status","url":…}`.
    subtype: Option<String>,
    url: Option<String>,
    /// Or `{"type":"attachment","attachment":{"type":"remote_session_change","url":…}}`.
    attachment: Option<Attachment>,
}

#[derive(Deserialize)]
struct Attachment {
    #[serde(rename = "type")]
    kind: Option<String>,
    url: Option<String>,
}

/// A session's link on claude.ai, where the owner continues it from any device.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Remote {
    pub session: String,
    pub url: String,
    pub at: i64,
    pub cwd: String,
}

/// Only Claude Code's own session links: nothing else from a transcript becomes a link.
pub fn is_session_url(url: &str) -> bool {
    url.strip_prefix("https://claude.ai/code/session_")
        .is_some_and(|id| !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// The session link a line announces, if it's a remote-control line.
fn remote_of(line: &Line) -> Option<&str> {
    let url = match (line.kind.as_str(), line.subtype.as_deref(), &line.attachment) {
        ("system", Some("bridge_status"), _) => line.url.as_deref()?,
        ("attachment", _, Some(Attachment { kind: Some(k), url: Some(u) })) if k == "remote_session_change" => u,
        _ => return None,
    };
    is_session_url(url).then_some(url)
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize, Serialize, Default)]
struct Usage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    cache_creation_input_tokens: i64,
    #[serde(default)]
    cache_read_input_tokens: i64,
}

/// The transcript folder Claude Code uses for a directory: every character that isn't a letter
/// or digit becomes a dash ("/home/viber/agent-flow" → "-home-viber-agent-flow").
pub fn folder_for(dir: &str) -> String {
    let dir = expand_home(dir.trim().trim_end_matches('/'));
    dir.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// The projects' checkouts (expanded) and their transcript folders.
struct Checkouts(Vec<(PathBuf, String, i64)>);

impl Checkouts {
    fn load(conn: &Connection) -> anyhow::Result<Self> {
        Ok(Self(
            crate::projects::list(conn)?
                .into_iter()
                .filter(|p| !p.checkout.trim().is_empty())
                .map(|p| (expand_home(p.checkout.trim()), folder_for(&p.checkout), p.id))
                .collect(),
        ))
    }

    /// A call's project: the checkout its working directory is in (the deepest, for nested ones),
    /// else the one whose folder the transcript is in. A project renamed since its session started
    /// keeps a folder of the old name, so the directory comes first.
    fn project(&self, cwd: &str, folder: &str) -> Option<i64> {
        let cwd = Path::new(cwd);
        self.0
            .iter()
            .filter(|(dir, _, _)| !cwd.as_os_str().is_empty() && cwd.starts_with(dir))
            .max_by_key(|(dir, _, _)| dir.as_os_str().len())
            .or_else(|| self.0.iter().find(|(_, f, _)| f == folder))
            .map(|(_, _, id)| *id)
    }
}

/// When the project's agent touched (created, changed, noted) which task, oldest first.
fn task_touches(conn: &Connection, project_id: i64) -> anyhow::Result<Vec<(i64, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT e.at, e.record_id FROM events e JOIN records r ON r.id = e.record_id
         JOIN projects p ON p.id = e.project_id
         WHERE e.project_id = ?1 AND r.kind = 'task' AND e.actor = p.slug
         ORDER BY e.at, e.seq",
    )?;
    let rows = stmt.query_map([project_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The task last touched at or before `at`.
fn task_at(touches: &[(i64, i64)], at: i64) -> Option<i64> {
    let i = touches.partition_point(|(t, _)| *t <= at);
    i.checked_sub(1).map(|i| touches[i].1)
}

fn parse_time(s: &str) -> Option<i64> {
    let t = time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()?;
    Some((t.unix_timestamp_nanos() / 1_000_000) as i64)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub files: usize,
    pub calls: usize,
}

/// Every transcript under `root`.
pub fn transcripts(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(list) = std::fs::read_dir(dir) else { return };
        for e in list.filter_map(Result::ok) {
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() {
                walk(&e.path(), out);
            } else if t.is_file() && e.path().extension().is_some_and(|x| x == "jsonl") {
                out.push(e.path());
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    // Sessions before subagents: a call a subagent's transcript repeats counts as its parent's.
    out.sort_by_cached_key(|p| (p.components().any(|c| c.as_os_str() == "subagents"), p.clone()));
    out
}

/// One model call, read from a transcript.
#[derive(Serialize, Deserialize)]
pub struct Call {
    id: String,
    at: i64,
    cwd: String,
    session: String,
    model: String,
    usage: Usage,
}

/// What's new in one transcript since `offset`. Reads files only, so it runs outside the
/// database's lock.
#[derive(Serialize, Deserialize)]
pub struct Read {
    /// The file: its path here, or for an upload the key the server keeps its offset under.
    pub path: String,
    pub folder: String,
    pub subagent: bool,
    calls: Vec<Call>,
    pub offset: u64,
    pub first_at: Option<i64>,
    #[serde(default)]
    remotes: Vec<Remote>,
    /// What sessions and subagents told each other (crate::sessionlog).
    #[serde(default)]
    log: crate::sessionlog::Found,
    /// The transcript's session and working directory, for the log.
    #[serde(default)]
    session: String,
    #[serde(default)]
    cwd: String,
}

/// Where the last round stopped in this file, and (for a subagent) the time of its first own call.
pub fn file_state(conn: &Connection, path: &Path) -> anyhow::Result<(u64, Option<i64>)> {
    Ok(conn
        .query_row("SELECT offset, first_at FROM usage_files WHERE path = ?1", [path.to_string_lossy()], |r| {
            Ok((r.get::<_, i64>(0)? as u64, r.get(1)?))
        })
        .optional()?
        .unwrap_or((0, None)))
}

/// The longest transcript line read; longer ones (no call is) are skipped.
const MAX_LINE: u64 = 8 * 1024 * 1024;
/// At most this much of one file per round; the rest waits for the next round.
const MAX_PER_ROUND: u64 = 512 * 1024 * 1024;
/// The most tokens one call can count in any field; anything else is a forged or broken line.
const MAX_TOKENS: i64 = 10_000_000;

/// Reads the whole lines after `offset` (None when there's nothing new). Bounded: lines over
/// MAX_LINE are skipped without being kept in memory, and one round reads at most MAX_PER_ROUND
/// of a file.
pub fn read_new(root: &Path, path: &Path, offset: u64, first_at: Option<i64>) -> anyhow::Result<Option<Read>> {
    let len = std::fs::metadata(path)?.len();
    // Rewritten from the start: read it again; the ids keep calls from counting twice.
    let mut offset = if len < offset { 0 } else { offset };
    if len == offset {
        return Ok(None);
    }
    let rel = path.strip_prefix(root).unwrap_or(path);
    let folder = rel.components().next().map(|c| c.as_os_str().to_string_lossy().to_string()).unwrap_or_default();
    let subagent = rel.components().any(|c| c.as_os_str() == "subagents");
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::new(file);
    let mut out = Read {
        path: path.to_string_lossy().to_string(),
        folder,
        subagent,
        calls: Vec::new(),
        offset,
        first_at,
        remotes: Vec::new(),
        log: crate::sessionlog::Found::default(),
        session: path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_owned(),
        cwd: String::new(),
    };
    let sub_info = if subagent { crate::sessionlog::subagent_of(path) } else { None };
    out.log.subagent = sub_info.clone();
    let start = offset;
    let mut buf = Vec::new();
    while offset - start < MAX_PER_ROUND {
        buf.clear();
        let n = (&mut reader).take(MAX_LINE).read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        if buf.last() != Some(&b'\n') {
            if (n as u64) < MAX_LINE {
                break; // the rest is a line still being written
            }
            // Too long to be a call: skip to the end of the line, a piece at a time.
            let mut skipped = n as u64;
            loop {
                buf.clear();
                let m = (&mut reader).take(MAX_LINE).read_until(b'\n', &mut buf)?;
                skipped += m as u64;
                if m == 0 || offset - start + skipped > MAX_PER_ROUND {
                    // Not finished yet; come back to it next round.
                    out.offset = offset;
                    return Ok(Some(out));
                }
                if buf.last() == Some(&b'\n') {
                    break;
                }
            }
            offset += skipped;
            continue;
        }
        offset += n as u64;
        crate::sessionlog::scan(&buf, sub_info.as_ref(), &mut out.log);
        if out.cwd.is_empty()
            && contains(&buf, b"\"cwd\":\"")
            && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf)
            && let Some(c) = v.get("cwd").and_then(|c| c.as_str())
        {
            out.cwd = c.to_owned();
        }
        // A session's own remote-control lines (a subagent has none of its own). Only lines that
        // are one: a model call that merely mentions the words still counts below.
        if !subagent
            && (contains(&buf, b"\"bridge_status\"") || contains(&buf, b"\"remote_session_change\""))
            && let Ok(line) = serde_json::from_slice::<Line>(&buf)
            && let Some(url) = remote_of(&line)
            && let Some(at) = line.timestamp.as_deref().and_then(parse_time)
        {
            out.remotes.push(Remote {
                session: line.session_id.clone().unwrap_or_default(),
                url: url.to_owned(),
                at,
                cwd: line.cwd.clone().unwrap_or_default(),
            });
            continue;
        }
        // Cheap checks first: most lines are tool results and user turns.
        if !contains(&buf, b"\"usage\"") || !contains(&buf, b"\"assistant\"") {
            continue;
        }
        let Ok(line) = serde_json::from_slice::<Line>(&buf) else { continue };
        let (Some(msg), Some(at)) = (line.message, line.timestamp.as_deref().and_then(parse_time)) else { continue };
        let (Some(id), Some(usage)) = (msg.id, msg.usage) else { continue };
        let counts =
            [usage.input_tokens, usage.output_tokens, usage.cache_creation_input_tokens, usage.cache_read_input_tokens];
        if counts.iter().any(|n| !(0..=MAX_TOKENS).contains(n)) || id.len() > 200 {
            continue;
        }
        if line.kind != "assistant" {
            continue;
        }
        out.calls.push(Call {
            id,
            at,
            cwd: line.cwd.unwrap_or_default(),
            session: line.session_id.unwrap_or_default(),
            model: msg.model.unwrap_or_default(),
            usage,
        });
    }
    out.offset = offset;
    Ok(Some(out))
}

/// Stores what was read and moves the file's offset; returns how many calls were new.
pub fn store(conn: &mut Connection, read: &Read) -> anyhow::Result<usize> {
    store_as(conn, read, None)
}

/// Like [`store`]; `project` given, everything read counts for that project (an upload from
/// `flow sync`, whose folders the server can't see).
pub fn store_as(conn: &mut Connection, read: &Read, project: Option<i64>) -> anyhow::Result<usize> {
    let checkouts = Checkouts::load(conn)?;
    let mut touches: HashMap<i64, Vec<(i64, i64)>> = HashMap::new();
    let tx = conn.transaction()?;
    let mut calls = 0;
    let mut first_at = read.first_at;
    for c in &read.calls {
        let project_id = project.or_else(|| checkouts.project(&c.cwd, &read.folder));
        // A subagent works for the task its session was on when it started: when it made its
        // first call of its own. Its transcript opens with copies of its parent's calls, which
        // carry the parent's older times.
        if read.subagent && first_at.is_none() {
            let repeated: bool =
                tx.query_row("SELECT EXISTS (SELECT 1 FROM usage WHERE message_id = ?1)", [&c.id], |r| r.get(0))?;
            if !repeated {
                first_at = Some(c.at);
            }
        }
        let when = if read.subagent { first_at.unwrap_or(c.at) } else { c.at };
        let task = match project_id {
            Some(p) => {
                if let std::collections::hash_map::Entry::Vacant(e) = touches.entry(p) {
                    e.insert(task_touches(&tx, p)?);
                }
                task_at(&touches[&p], when)
            }
            None => None,
        };
        calls += tx.execute(
            "INSERT OR IGNORE INTO usage (message_id, at, folder, cwd, session, subagent, model, project_id, task_id,
                                          input, output, cache_write, cache_read)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                c.id,
                c.at,
                read.folder,
                c.cwd,
                c.session,
                read.subagent,
                c.model,
                project_id,
                task,
                c.usage.input_tokens,
                c.usage.output_tokens,
                c.usage.cache_creation_input_tokens,
                c.usage.cache_read_input_tokens
            ],
        )?;
    }
    if !read.log.is_empty() {
        let cwd = if read.cwd.is_empty() {
            read.calls.first().map(|c| c.cwd.as_str()).unwrap_or("")
        } else {
            read.cwd.as_str()
        };
        let project_id = project.or_else(|| checkouts.project(cwd, &read.folder));
        crate::sessionlog::store(&tx, &read.log, &read.session, &read.folder, cwd, project_id)?;
    }
    for r in &read.remotes {
        tx.execute(
            "INSERT INTO remote_sessions (session, url, at, folder, cwd) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (session, url) DO UPDATE SET at = max(at, excluded.at)",
            params![r.session, r.url, r.at, read.folder, r.cwd],
        )?;
    }
    tx.execute(
        "INSERT INTO usage_files (path, offset, first_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (path) DO UPDATE SET offset = excluded.offset, first_at = coalesce(first_at, excluded.first_at)",
        params![read.path, read.offset as i64, first_at],
    )?;
    tx.commit()?;
    Ok(calls)
}

/// The newest link to a session of the project's agent, and when it was announced: the session
/// worked in the project's checkout (the deepest one, for nested checkouts), or its transcripts are
/// in the checkout's folder.
pub fn session_link(conn: &Connection, project_id: i64) -> anyhow::Result<Option<(String, i64)>> {
    let checkouts = Checkouts::load(conn)?;
    let Some((dir, folder, _)) = checkouts.0.iter().find(|(_, _, id)| *id == project_id) else {
        return Ok(None);
    };
    let dir = dir.to_string_lossy().to_string();
    let mut stmt = conn.prepare(
        "SELECT url, at, cwd, folder FROM remote_sessions
         WHERE cwd = ?1 OR substr(cwd, 1, length(?1) + 1) = ?1 || '/' OR folder = ?2
         ORDER BY at DESC LIMIT 50",
    )?;
    let rows = stmt.query_map(params![dir, folder], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
    })?;
    for row in rows {
        let (url, at, cwd, folder) = row?;
        if checkouts.project(&cwd, &folder) == Some(project_id) && is_session_url(&url) {
            return Ok(Some((url, at)));
        }
    }
    Ok(None)
}

/// Calls without a project get one when they can: a checkout set since they were read, or the
/// project the rest of their session works for (one agent per project, so a session is one
/// project's, even the part spent in a directory of an older name).
pub fn assign_projects(conn: &Connection) -> anyhow::Result<()> {
    // Deepest checkouts first, as for new calls, so a nested checkout keeps its own calls.
    let mut checkouts = Checkouts::load(conn)?.0;
    checkouts.sort_by_key(|(dir, _, _)| std::cmp::Reverse(dir.as_os_str().len()));
    for (dir, folder, id) in checkouts {
        let dir = dir.to_string_lossy().to_string();
        conn.execute(
            "UPDATE usage SET project_id = ?1 WHERE project_id IS NULL
               AND (cwd = ?2 OR substr(cwd, 1, length(?2) + 1) = ?2 || '/' OR (cwd = '' AND folder = ?3))",
            params![id, dir, folder],
        )?;
    }
    // Sessions with calls both with and without a project: one grouped read, then an indexed
    // update per session (a correlated UPDATE over every row took seconds, holding the lock).
    let mut stmt = conn.prepare(
        "SELECT session, project_id, count(*) FROM usage
         WHERE project_id IS NOT NULL
           AND session IN (SELECT DISTINCT session FROM usage WHERE project_id IS NULL AND session != '')
         GROUP BY session, project_id",
    )?;
    let mut best: HashMap<String, (i64, i64)> = HashMap::new();
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))? {
        let (session, project, n) = row?;
        let e = best.entry(session).or_insert((project, 0));
        if n > e.1 {
            *e = (project, n);
        }
    }
    for (session, (project, _)) in best {
        conn.execute(
            "UPDATE usage SET project_id = ?1 WHERE session = ?2 AND project_id IS NULL",
            params![project, session],
        )?;
    }
    Ok(())
}

/// Everything at once, holding the connection (tests; the server reads files outside the lock).
#[cfg(test)]
pub fn ingest(conn: &mut Connection, root: &Path) -> anyhow::Result<Stats> {
    let mut stats = Stats::default();
    for path in transcripts(root) {
        let (offset, first_at) = file_state(conn, &path)?;
        if let Some(read) = read_new(root, &path, offset, first_at)? {
            let n = store(conn, &read)?;
            if n > 0 {
                stats.files += 1;
                stats.calls += n;
            }
        }
    }
    assign_projects(conn)?;
    Ok(stats)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Where transcripts are, from the config.
pub fn root(config: &crate::config::Config) -> PathBuf {
    expand_home(&config.transcripts_dir)
}

// ---------------------------------------------------------------------------------------------
// Queries for the pages

/// Tokens of one bucket: "new" is input + cache writes + output; cache reads are counted apart
/// (they cost a tenth, and are ~60× as many).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tokens {
    pub calls: i64,
    pub input: i64,
    pub cache_write: i64,
    pub output: i64,
    pub cache_read: i64,
}

impl Tokens {
    pub fn new_tokens(&self) -> i64 {
        self.input + self.cache_write + self.output
    }

    pub fn add(&mut self, o: &Tokens) {
        self.calls += o.calls;
        self.input += o.input;
        self.cache_write += o.cache_write;
        self.output += o.output;
        self.cache_read += o.cache_read;
    }
}

fn tokens_row(r: &rusqlite::Row, from: usize) -> rusqlite::Result<Tokens> {
    Ok(Tokens {
        calls: r.get(from)?,
        input: r.get(from + 1)?,
        cache_write: r.get(from + 2)?,
        output: r.get(from + 3)?,
        cache_read: r.get(from + 4)?,
    })
}

const SUMS: &str = "count(*), sum(input), sum(cache_write), sum(output), sum(cache_read)";

/// Counted in quarter hours, which every time zone's hours and days are made of.
const QUARTER: i64 = 15 * 60_000;

/// Totals per (bucket, project) for the buckets between consecutive `bounds` (the owner's local
/// hours or days, see [`crate::tz`]). Project None is everything not in a project.
pub fn buckets(conn: &Connection, bounds: &[i64]) -> anyhow::Result<Vec<(usize, Option<i64>, Tokens)>> {
    let (Some(&from), Some(&to)) = (bounds.first(), bounds.last()) else { return Ok(Vec::new()) };
    let mut stmt = conn.prepare(&format!(
        "SELECT (at - ?1) / ?3, project_id, {SUMS} FROM usage WHERE at >= ?1 AND at < ?2 GROUP BY 1, 2"
    ))?;
    let rows = stmt.query_map(params![from, to, QUARTER], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?, tokens_row(r, 2)?))
    })?;
    let mut by: std::collections::BTreeMap<(usize, Option<i64>), Tokens> = Default::default();
    for row in rows {
        let (q, project, t) = row?;
        if let Some(i) = crate::tz::bucket(bounds, from + q * QUARTER) {
            by.entry((i, project)).or_default().add(&t);
        }
    }
    Ok(by.into_iter().map(|((i, p), t)| (i, p, t)).collect())
}

/// One row per project (None: outside any project) for the window, with its session count.
pub fn per_project(conn: &Connection, from: i64, to: i64) -> anyhow::Result<Vec<(Option<i64>, i64, Tokens)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT project_id, count(DISTINCT session), {SUMS} FROM usage WHERE at >= ?1 AND at < ?2 GROUP BY 1"
    ))?;
    let rows = stmt.query_map(params![from, to], |r| Ok((r.get(0)?, r.get(1)?, tokens_row(r, 2)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Per task in the window: (task id or None for "not in a task", project, main session, subagents).
pub struct TaskUsage {
    pub task_id: Option<i64>,
    pub project_id: Option<i64>,
    pub main: Tokens,
    pub subagents: Tokens,
    pub first_at: i64,
    pub last_at: i64,
}

impl TaskUsage {
    pub fn total(&self) -> Tokens {
        let mut t = self.main;
        t.add(&self.subagents);
        t
    }
}

pub fn per_task(conn: &Connection, from: i64, to: i64) -> anyhow::Result<Vec<TaskUsage>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT task_id, project_id, subagent, min(at), max(at), {SUMS} FROM usage
         WHERE at >= ?1 AND at < ?2 AND project_id IS NOT NULL GROUP BY 1, 2, 3"
    ))?;
    type Row = (Option<i64>, Option<i64>, bool, i64, i64, Tokens);
    let rows: Vec<Row> = stmt
        .query_map(params![from, to], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, tokens_row(r, 5)?))
        })?
        .collect::<Result<_, _>>()?;
    let mut by: Vec<TaskUsage> = Vec::new();
    for (task, project, sub, first, last, t) in rows {
        let i = match by.iter().position(|x| x.task_id == task && x.project_id == project) {
            Some(i) => i,
            None => {
                by.push(TaskUsage {
                    task_id: task,
                    project_id: project,
                    main: Tokens::default(),
                    subagents: Tokens::default(),
                    first_at: first,
                    last_at: last,
                });
                by.len() - 1
            }
        };
        let e = &mut by[i];
        if sub {
            e.subagents.add(&t)
        } else {
            e.main.add(&t)
        }
        e.first_at = e.first_at.min(first);
        e.last_at = e.last_at.max(last);
    }
    by.sort_by_key(|t| std::cmp::Reverse(t.total().new_tokens()));
    Ok(by)
}

/// A project's new tokens per bucket of `bounds` (the agent card's strip of local days).
pub fn project_days(conn: &Connection, project_id: i64, bounds: &[i64]) -> anyhow::Result<Vec<i64>> {
    let (Some(&from), Some(&to)) = (bounds.first(), bounds.last()) else { return Ok(Vec::new()) };
    let mut out = vec![0; bounds.len() - 1];
    let mut stmt = conn.prepare(
        "SELECT (at - ?2) / ?4, sum(input + cache_write + output) FROM usage
         WHERE project_id = ?1 AND at >= ?2 AND at < ?3 GROUP BY 1",
    )?;
    for row in
        stmt.query_map(params![project_id, from, to, QUARTER], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
    {
        let (q, n) = row?;
        if let Some(i) = crate::tz::bucket(bounds, from + q * QUARTER) {
            out[i] += n;
        }
    }
    Ok(out)
}

/// "1.2 M", "34 k", "980".
pub fn short(n: i64) -> String {
    let f = n as f64;
    if f >= 1e9 {
        format!("{:.1} B", f / 1e9)
    } else if f >= 1e6 {
        format!("{:.1} M", f / 1e6)
    } else if f >= 1e3 {
        format!("{:.0} k", f / 1e3)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn call(id: &str, at: &str, session: &str, input: i64) -> String {
        format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{at}\",\"sessionId\":\"{session}\",\"message\":{{\"id\":\"{id}\",\"model\":\"claude-opus-5-5\",\"role\":\"assistant\",\"usage\":{{\"input_tokens\":{input},\"output_tokens\":10,\"cache_creation_input_tokens\":100,\"cache_read_input_tokens\":1000}}}}}}\n"
        )
    }

    #[test]
    fn session_links_come_from_remote_control_lines_only() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("-home-viber-office");
        std::fs::create_dir_all(folder.join("s1/subagents")).unwrap();
        let bridge = |url: &str, at: &str| {
            format!(
                "{{\"type\":\"system\",\"subtype\":\"bridge_status\",\"url\":\"{url}\",\"timestamp\":\"{at}\",\"sessionId\":\"s1\",\"cwd\":\"/home/viber/office\"}}\n"
            )
        };
        let change = |url: &str, at: &str| {
            format!(
                "{{\"type\":\"attachment\",\"attachment\":{{\"type\":\"remote_session_change\",\"url\":\"{url}\"}},\"timestamp\":\"{at}\",\"sessionId\":\"s1\",\"cwd\":\"/home/viber/office\"}}\n"
            )
        };
        // A model call that only mentions the words, and a link to somewhere else.
        let mention =
            call("m1", "2026-11-11T09:30:00Z", "s1", 5).replace("\"role\"", "\"note\":\"bridge_status\",\"role\"");
        let text = [
            bridge("https://claude.ai/code/session_01Old", "2026-11-11T08:00:00Z"),
            mention,
            change("https://claude.ai/code/session_01New", "2026-11-11T10:00:00Z"),
            bridge("https://evil.example/code/session_01X", "2026-11-11T11:00:00Z"),
            change("https://claude.ai/code/session_01New\\\"><script>", "2026-11-11T12:00:00Z"),
        ]
        .concat();
        std::fs::write(folder.join("s1.jsonl"), text).unwrap();
        // A subagent's copy of its parent's lines isn't its own.
        std::fs::write(
            folder.join("s1/subagents/a.jsonl"),
            bridge("https://claude.ai/code/session_01Sub", "2026-11-11T13:00:00Z"),
        )
        .unwrap();

        let db = Db::open_memory();
        db.call_blocking(|c| {
            let p = crate::projects::create(c, "office", "Office", "t")?;
            let patch =
                crate::projects::ProjectPatch { checkout: Some("/home/viber/office".into()), ..Default::default() };
            crate::projects::update(c, p.id, &patch, "t")?;
            for path in transcripts(root.path()) {
                let (offset, first_at) = file_state(c, &path)?;
                if let Some(read) = read_new(root.path(), &path, offset, first_at)? {
                    store(c, &read)?;
                }
            }
            let (url, _) = session_link(c, p.id)?.unwrap();
            assert_eq!(url, "https://claude.ai/code/session_01New");
            let calls: i64 = c.query_row("SELECT count(*) FROM usage", [], |r| r.get(0))?;
            assert_eq!(calls, 1, "the call that mentions the words still counts");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn transcripts_are_read_once_and_attributed() {
        let root = tempfile::tempdir().unwrap();
        let folder = root.path().join("-home-viber-office");
        std::fs::create_dir_all(folder.join("s1/subagents")).unwrap();
        let main = folder.join("s1.jsonl");
        let user = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n";
        std::fs::write(&main, format!("{user}{}", call("m1", "2026-11-11T01:00:00Z", "s1", 5))).unwrap();
        // The subagent repeats its parent's first call and adds its own.
        std::fs::write(
            folder.join("s1/subagents/agent-a.jsonl"),
            format!("{}{}", call("m1", "2026-11-11T01:00:00Z", "s1", 5), call("m2", "2026-11-11T03:00:00Z", "s1", 7)),
        )
        .unwrap();

        let db = Db::open_memory();
        db.call_blocking(move |c| {
            let p = crate::projects::create(c, "imaginarium", "Imaginarium", "t")?;
            let patch =
                crate::projects::ProjectPatch { checkout: Some("/home/viber/office".into()), ..Default::default() };
            crate::projects::update(c, p.id, &patch, "t")?;
            // The agent works on T-1 from 00:30, then on T-2 from 02:00.
            let t1 = crate::records::create(
                c,
                p.id,
                "imaginarium",
                crate::records::NewRecord {
                    kind: Some(crate::records::Kind::Task),
                    title: "Tables".into(),
                    ..Default::default()
                },
            )?;
            let t2 = crate::records::create(
                c,
                p.id,
                "imaginarium",
                crate::records::NewRecord {
                    kind: Some(crate::records::Kind::Task),
                    title: "Print".into(),
                    ..Default::default()
                },
            )?;
            let at = |s: &str| parse_time(s).unwrap();
            c.execute("UPDATE events SET at = ?1 WHERE record_id = ?2", params![at("2026-11-11T00:30:00Z"), t1.id])?;
            c.execute("UPDATE events SET at = ?1 WHERE record_id = ?2", params![at("2026-11-11T02:00:00Z"), t2.id])?;

            let s = ingest(c, root.path())?;
            assert_eq!(s.calls, 2, "m1 once, m2 once");
            assert_eq!(ingest(c, root.path())?, Stats::default(), "nothing new");
            // A line still being written is left for the next round.
            let mut f = std::fs::OpenOptions::new().append(true).open(&main)?;
            use std::io::Write;
            let next = call("m3", "2026-11-11T04:00:00Z", "s1", 9);
            f.write_all(&next.as_bytes()[..40])?;
            assert_eq!(ingest(c, root.path())?.calls, 0);
            f.write_all(&next.as_bytes()[40..])?;
            assert_eq!(ingest(c, root.path())?.calls, 1);

            let task_of = |id: &str| -> Option<i64> {
                c.query_row("SELECT task_id FROM usage WHERE message_id = ?1", [id], |r| r.get(0)).unwrap()
            };
            assert_eq!(task_of("m1"), Some(t1.id));
            assert_eq!(task_of("m3"), Some(t2.id), "the session moved on to T-2");
            // The subagent's transcript opens with a copy of its parent's 01:00 call; its own first
            // call is at 03:00, when the session was on T-2.
            assert_eq!(task_of("m2"), Some(t2.id));

            let tasks = per_task(c, 0, i64::MAX)?;
            let first = tasks.iter().find(|t| t.task_id == Some(t1.id)).unwrap();
            assert_eq!((first.main.calls, first.subagents.calls), (1, 0));
            assert_eq!(first.total().new_tokens(), 5 + 110);
            let second = tasks.iter().find(|t| t.task_id == Some(t2.id)).unwrap();
            assert_eq!((second.main.calls, second.subagents.calls), (1, 1), "m3, and the subagent's m2");
            let rows = per_project(c, 0, i64::MAX)?;
            assert_eq!(
                rows,
                vec![(Some(p.id), 1, Tokens { calls: 3, input: 21, cache_write: 300, output: 30, cache_read: 3000 })]
            );

            // A session started in the home folder works in the checkout: the directory decides.
            // One started under the project's old name belongs to it through its session.
            let home = root.path().join("-home-viber");
            std::fs::create_dir_all(&home)?;
            let with_cwd = |id: &str, session: &str, cwd: &str| {
                call(id, "2026-11-11T05:00:00Z", session, 1).replace("\"type\"", &format!("\"cwd\":\"{cwd}\",\"type\""))
            };
            std::fs::write(home.join("s2.jsonl"), with_cwd("m4", "s2", "/home/viber/office/crates/app"))?;
            let old = root.path().join("-home-viber-oldname");
            std::fs::create_dir_all(&old)?;
            std::fs::write(
                old.join("s3.jsonl"),
                format!(
                    "{}{}",
                    with_cwd("m5", "s3", "/home/viber/oldname"),
                    with_cwd("m6", "s3", "/home/viber/office")
                ),
            )?;
            ingest(c, root.path())?;
            let project_of = |id: &str| -> Option<i64> {
                c.query_row("SELECT project_id FROM usage WHERE message_id = ?1", [id], |r| r.get(0)).unwrap()
            };
            assert_eq!((project_of("m4"), project_of("m5"), project_of("m6")), (Some(p.id), Some(p.id), Some(p.id)));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn long_lines_and_forged_counts_are_skipped() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("-home-viber-x");
        std::fs::create_dir_all(&dir).unwrap();
        let huge = format!("{{\"type\":\"user\",\"pad\":\"{}\"}}\n", "x".repeat(MAX_LINE as usize + 10));
        let forged = call("bad", "2026-11-11T01:00:00Z", "s", 9_000_000_000_000_000_000);
        let good = call("ok", "2026-11-11T01:00:00Z", "s", 3);
        std::fs::write(dir.join("s.jsonl"), format!("{huge}{forged}{good}")).unwrap();
        let read = read_new(root.path(), &dir.join("s.jsonl"), 0, None).unwrap().unwrap();
        assert_eq!(read.calls.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["ok"]);
        assert_eq!(read.offset, std::fs::metadata(dir.join("s.jsonl")).unwrap().len());
    }

    #[test]
    fn folders_and_numbers() {
        assert_eq!(folder_for("/home/viber/agent-flow/"), "-home-viber-agent-flow");
        assert_eq!(folder_for("/home/viber/my.app"), "-home-viber-my-app");
        assert_eq!(short(1_234_567), "1.2 M");
        assert_eq!(short(34_000), "34 k");
    }
}
