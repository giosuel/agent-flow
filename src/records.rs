//! Typed records: questions, decisions, tasks, ADRs (later PRs, builds, feedback). Each has a
//! stable id like `vinculum/Q-12`; every change appends a revision; links connect records.

use std::fmt;

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::util::now_ms;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Question,
    Decision,
    Task,
    Adr,
    Pr,
    Release,
    Build,
    Feedback,
    Contribution,
}

impl Kind {
    pub const ALL: [Kind; 9] = [
        Kind::Question,
        Kind::Decision,
        Kind::Task,
        Kind::Adr,
        Kind::Pr,
        Kind::Release,
        Kind::Build,
        Kind::Feedback,
        Kind::Contribution,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Question => "question",
            Kind::Decision => "decision",
            Kind::Task => "task",
            Kind::Adr => "adr",
            Kind::Pr => "pr",
            Kind::Release => "release",
            Kind::Build => "build",
            Kind::Feedback => "feedback",
            Kind::Contribution => "contribution",
        }
    }

    /// The letters in a record's id: vinculum/Q-12, vinculum/D-22, vinculum/T-14.
    pub fn prefix(self) -> &'static str {
        match self {
            Kind::Question => "Q",
            Kind::Decision => "D",
            Kind::Task => "T",
            Kind::Adr => "ADR",
            Kind::Pr => "PR",
            Kind::Release => "R",
            Kind::Build => "B",
            Kind::Feedback => "F",
            Kind::Contribution => "C",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Question => "Question",
            Kind::Decision => "Decision",
            Kind::Task => "Task",
            Kind::Adr => "ADR",
            Kind::Pr => "Pull request",
            Kind::Release => "Release",
            Kind::Build => "Build",
            Kind::Feedback => "Feedback",
            Kind::Contribution => "External contribution",
        }
    }

    pub fn parse(s: &str) -> anyhow::Result<Kind> {
        Kind::ALL
            .into_iter()
            .find(|k| k.as_str().eq_ignore_ascii_case(s) || k.prefix().eq_ignore_ascii_case(s))
            .with_context(|| format!("unknown record kind {s:?}"))
    }

    /// The states a record of this kind can be in; the first is where it starts.
    pub fn states(self) -> &'static [&'static str] {
        match self {
            Kind::Question => &["open", "answered", "withdrawn"],
            Kind::Decision | Kind::Adr => &["current", "replaced"],
            Kind::Task => &["todo", "doing", "review", "done", "blocked", "parked"],
            Kind::Pr => &["open", "merged", "closed"],
            Kind::Release => &["planned", "waiting", "tagged", "dropped"],
            Kind::Build => &["published", "tried", "withdrawn"],
            Kind::Feedback => &["works", "problem", "skipped"],
            Kind::Contribution => &["draft", "shown", "approved", "sent", "merged", "closed"],
        }
    }

    pub fn check_state(self, state: &str) -> anyhow::Result<()> {
        if !self.states().contains(&state) {
            bail!("a {} is one of: {}", self.as_str(), self.states().join(", "));
        }
        Ok(())
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Has the record reached the end its kind works towards (a PR merged, a task done, …)?
pub fn is_finished(r: &Record) -> bool {
    r.state == finished_word(r.kind)
}

/// The state that counts as finished for a kind, for "unlocks once T-1 is done".
pub fn finished_word(kind: Kind) -> &'static str {
    match kind {
        Kind::Pr => "merged",
        Kind::Release => "tagged",
        Kind::Question => "answered",
        Kind::Build => "tried",
        Kind::Contribution => "merged",
        Kind::Decision | Kind::Adr => "current",
        Kind::Feedback => "works",
        Kind::Task => "done",
    }
}

/// How the owner sees a record's id: GitHub's "#58" for PRs, "T-14" otherwise.
pub fn display_id(r: &Record) -> String {
    if r.kind == Kind::Pr { format!("#{}", r.number) } else { r.short_id() }
}

/// `vinculum/Q-12`, or `Q-12` within the caller's own project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordRef {
    pub project: Option<String>,
    pub kind: Kind,
    pub number: i64,
}

impl RecordRef {
    pub fn parse(s: &str) -> anyhow::Result<RecordRef> {
        let s = s.trim().trim_start_matches('#');
        let (project, rest) = match s.rsplit_once('/') {
            Some((p, r)) => (Some(p.trim().to_owned()), r),
            None => (None, s),
        };
        let (prefix, number) =
            rest.rsplit_once('-').with_context(|| format!("{s:?} isn't a record id like vinculum/Q-12"))?;
        let number: i64 =
            number.trim().parse().with_context(|| format!("{s:?} isn't a record id like vinculum/Q-12"))?;
        Ok(RecordRef { project, kind: Kind::parse(prefix.trim())?, number })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Record {
    #[serde(skip)]
    pub id: i64,
    /// The stable id agents and pages use, e.g. `vinculum/Q-12`.
    #[serde(rename = "id")]
    pub ref_id: String,
    pub project: String,
    #[serde(skip)]
    pub project_id: i64,
    pub kind: Kind,
    pub number: i64,
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub body: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub milestone: Option<String>,
    #[serde(skip)]
    pub milestone_id: Option<i64>,
    #[serde(skip)]
    pub position: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "is_empty_object")]
    pub data: Value,
    pub author: String,
    pub created_at: i64,
    pub updated_at: i64,
}

fn is_empty_object(v: &Value) -> bool {
    v.as_object().is_some_and(Map::is_empty)
}

impl Record {
    pub fn short_id(&self) -> String {
        format!("{}-{}", self.kind.prefix(), self.number)
    }

    pub fn url(&self) -> String {
        format!("/p/{}/{}", self.project, self.short_id())
    }

    pub fn str(&self, key: &str) -> &str {
        self.data.get(key).and_then(Value::as_str).unwrap_or("")
    }
}

const RECORD_SELECT: &str =
    "SELECT r.id, p.slug, r.project_id, r.kind, r.number, r.title, r.body, r.state, m.name, r.milestone_id,
       r.position, r.tags, r.data, r.author, r.created_at, r.updated_at
     FROM records r JOIN projects p ON p.id = r.project_id LEFT JOIN milestones m ON m.id = r.milestone_id";

fn record_from_row(r: &Row) -> rusqlite::Result<Record> {
    let kind: String = r.get(3)?;
    let kind = Kind::parse(&kind)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, e.into()))?;
    let project: String = r.get(1)?;
    let number: i64 = r.get(4)?;
    let tags: String = r.get(11)?;
    Ok(Record {
        id: r.get(0)?,
        ref_id: format!("{project}/{}-{number}", kind.prefix()),
        project,
        project_id: r.get(2)?,
        kind,
        number,
        title: r.get(5)?,
        body: r.get(6)?,
        state: r.get(7)?,
        milestone: r.get(8)?,
        milestone_id: r.get(9)?,
        position: r.get(10)?,
        tags: tags.split(',').filter(|t| !t.is_empty()).map(str::to_owned).collect(),
        data: serde_json::from_str(&r.get::<_, String>(12)?).unwrap_or(json!({})),
        author: r.get(13)?,
        created_at: r.get(14)?,
        updated_at: r.get(15)?,
    })
}

pub fn get(conn: &Connection, id: i64) -> anyhow::Result<Record> {
    Ok(conn.query_row(&format!("{RECORD_SELECT} WHERE r.id = ?1"), [id], record_from_row)?)
}

/// Looks up `vinculum/Q-12` (or `Q-12` in `default_project`).
pub fn find(conn: &Connection, r: &RecordRef, default_project: Option<&str>) -> anyhow::Result<Record> {
    let project = r.project.as_deref().or(default_project).context("say which project, e.g. vinculum/Q-12")?;
    try_find(conn, r, default_project)?.with_context(|| format!("no record {project}/{}-{}", r.kind.prefix(), r.number))
}

/// Like `find`, but "no such record" is `None` and only real failures are errors.
pub fn try_find(conn: &Connection, r: &RecordRef, default_project: Option<&str>) -> anyhow::Result<Option<Record>> {
    let project = r.project.as_deref().or(default_project).context("say which project, e.g. vinculum/Q-12")?;
    Ok(conn
        .query_row(
            &format!("{RECORD_SELECT} WHERE p.slug = ?1 AND r.kind = ?2 AND r.number = ?3"),
            params![project, r.kind.as_str(), r.number],
            record_from_row,
        )
        .optional()?)
}

pub fn find_str(conn: &Connection, id: &str, default_project: Option<&str>) -> anyhow::Result<Record> {
    find(conn, &RecordRef::parse(id)?, default_project)
}

fn normalize_tags(tags: &[String]) -> String {
    let mut clean: Vec<String> =
        tags.iter().map(|t| t.trim().to_lowercase().replace([' ', ','], "-")).filter(|t| !t.is_empty()).collect();
    clean.sort();
    clean.dedup();
    if clean.is_empty() { String::new() } else { format!(",{},", clean.join(",")) }
}

#[derive(Debug, Default)]
pub struct NewRecord {
    pub kind: Option<Kind>,
    /// A fixed number instead of the next free one: a PR keeps its GitHub number (PR-58 is #58).
    pub number: Option<i64>,
    pub title: String,
    pub body: String,
    /// None: the kind's first state.
    pub state: Option<String>,
    pub milestone_id: Option<i64>,
    pub tags: Vec<String>,
    pub data: Map<String, Value>,
}

/// Creates a record with revision 1 and an event. The caller decides who may.
pub fn create(conn: &Connection, project_id: i64, author: &str, new: NewRecord) -> anyhow::Result<Record> {
    let kind = new.kind.context("record kind missing")?;
    let title = new.title.trim();
    if title.is_empty() {
        bail!("a {kind} needs a title");
    }
    if title.chars().count() > 300 {
        bail!("keep the title under 300 characters; put the rest in the body");
    }
    let state = new.state.unwrap_or_else(|| kind.states()[0].to_owned());
    kind.check_state(&state)?;
    let now = now_ms();
    let number: i64 = match new.number {
        Some(n) => n,
        None => conn.query_row(
            "SELECT coalesce(max(number), 0) + 1 FROM records WHERE project_id = ?1 AND kind = ?2",
            params![project_id, kind.as_str()],
            |r| r.get(0),
        )?,
    };
    let position: Option<f64> = match kind {
        Kind::Task => Some(number as f64),
        _ => None,
    };
    conn.execute(
        "INSERT INTO records (project_id, kind, number, title, body, state, milestone_id, position, tags, data, author, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
        params![
            project_id,
            kind.as_str(),
            number,
            title,
            new.body.trim(),
            state,
            new.milestone_id,
            position,
            normalize_tags(&new.tags),
            Value::Object(new.data.clone()).to_string(),
            author,
            now
        ],
    )?;
    let id = conn.last_insert_rowid();
    let mut snapshot = new.data;
    snapshot.insert("title".into(), title.into());
    snapshot.insert("state".into(), state.clone().into());
    if !new.body.trim().is_empty() {
        snapshot.insert("body".into(), new.body.trim().into());
    }
    conn.execute(
        "INSERT INTO revisions (record_id, rev, at, author, changes, note) VALUES (?1, 1, ?2, ?3, ?4, 'created')",
        params![id, now, author, Value::Object(snapshot).to_string()],
    )?;
    let record = get(conn, id)?;
    event(
        conn,
        Some(project_id),
        Some(id),
        &format!("{kind}.created"),
        &format!("{} {}: {}", kind.label(), record.short_id(), title),
        author,
    )?;
    Ok(record)
}

/// A change to a record. Fields left None stay as they are; `data` keys are merged in (a null
/// value removes the key).
#[derive(Debug, Default)]
pub struct Patch {
    pub title: Option<String>,
    pub body: Option<String>,
    pub state: Option<String>,
    pub milestone_id: Option<Option<i64>>,
    pub position: Option<f64>,
    pub tags: Option<Vec<String>>,
    pub data: Map<String, Value>,
    pub note: String,
    /// Skip the generic "updated" event because the caller records a clearer one.
    pub quiet: bool,
}

/// Applies a patch, appends a revision with what changed, and records an event. Returns the
/// record and whether anything changed.
pub fn update(conn: &Connection, record: &Record, author: &str, patch: Patch) -> anyhow::Result<(Record, bool)> {
    let mut changes = Map::new();
    let mut title = record.title.clone();
    let mut body = record.body.clone();
    let mut state = record.state.clone();
    let mut milestone_id = record.milestone_id;
    let mut position = record.position;
    let mut tags = normalize_tags(&record.tags);
    let mut data = record.data.as_object().cloned().unwrap_or_default();

    if let Some(t) = patch.title.map(|t| t.trim().to_owned())
        && t != title
    {
        if t.is_empty() {
            bail!("the title can't be empty");
        }
        changes.insert("title".into(), t.clone().into());
        title = t;
    }
    if let Some(b) = patch.body.map(|b| b.trim().to_owned())
        && b != body
    {
        changes.insert("body".into(), b.clone().into());
        body = b;
    }
    if let Some(s) = patch.state
        && s != state
    {
        record.kind.check_state(&s)?;
        changes.insert("state".into(), s.clone().into());
        state = s;
    }
    if let Some(m) = patch.milestone_id
        && m != milestone_id
    {
        let name = match m {
            Some(id) => conn.query_row("SELECT name FROM milestones WHERE id = ?1", [id], |r| r.get::<_, String>(0))?,
            None => String::new(),
        };
        changes.insert("milestone".into(), name.into());
        milestone_id = m;
    }
    if let Some(p) = patch.position
        && Some(p) != position
    {
        changes.insert("position".into(), p.into());
        position = Some(p);
    }
    if let Some(t) = patch.tags {
        let t = normalize_tags(&t);
        if t != tags {
            changes.insert("tags".into(), t.trim_matches(',').into());
            tags = t;
        }
    }
    for (k, v) in patch.data {
        if data.get(&k) != Some(&v) && !(v.is_null() && !data.contains_key(&k)) {
            changes.insert(k.clone(), v.clone());
            if v.is_null() {
                data.remove(&k);
            } else {
                data.insert(k, v);
            }
        }
    }
    if changes.is_empty() && patch.note.trim().is_empty() {
        return Ok((record.clone(), false));
    }
    let now = now_ms();
    conn.execute(
        "UPDATE records SET title = ?1, body = ?2, state = ?3, milestone_id = ?4, position = ?5, tags = ?6, data = ?7, updated_at = ?8
         WHERE id = ?9",
        params![title, body, state, milestone_id, position, tags, Value::Object(data).to_string(), now, record.id],
    )?;
    let rev: i64 =
        conn.query_row("SELECT coalesce(max(rev), 0) + 1 FROM revisions WHERE record_id = ?1", [record.id], |r| {
            r.get(0)
        })?;
    conn.execute(
        "INSERT INTO revisions (record_id, rev, at, author, changes, note) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![record.id, rev, now, author, Value::Object(changes.clone()).to_string(), patch.note.trim()],
    )?;
    if patch.quiet {
        return Ok((get(conn, record.id)?, true));
    }
    let what = describe_changes(&changes, &patch.note);
    event(
        conn,
        Some(record.project_id),
        Some(record.id),
        &format!("{}.updated", record.kind),
        &format!("{} {}: {what}", record.kind.label(), record.short_id()),
        author,
    )?;
    Ok((get(conn, record.id)?, true))
}

fn describe_changes(changes: &Map<String, Value>, note: &str) -> String {
    let mut parts: Vec<String> = changes
        .iter()
        .filter(|(k, _)| !k.ends_with("_at"))
        .map(|(k, v)| match (k.as_str(), v) {
            ("state", Value::String(s)) => format!("now {s}"),
            ("milestone", Value::String(m)) if !m.is_empty() => format!("moved to {m}"),
            (k, _) => format!("{k} changed"),
        })
        .collect();
    if !note.trim().is_empty() {
        parts.push(note.trim().to_owned());
    }
    parts.join(", ")
}

#[derive(Debug, Clone, Serialize)]
pub struct Revision {
    pub rev: i64,
    pub at: i64,
    pub author: String,
    pub changes: Value,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub note: String,
}

pub fn revisions(conn: &Connection, record_id: i64) -> anyhow::Result<Vec<Revision>> {
    let mut stmt =
        conn.prepare("SELECT rev, at, author, changes, note FROM revisions WHERE record_id = ?1 ORDER BY rev DESC")?;
    let rows = stmt.query_map([record_id], |r| {
        Ok(Revision {
            rev: r.get(0)?,
            at: r.get(1)?,
            author: r.get(2)?,
            changes: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or(Value::Null),
            note: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The owner answers a question: a numbered option, their own words, or both. The words are
/// stored exactly as typed.
pub fn answer_question(
    conn: &Connection,
    q: &Record,
    option: Option<u32>,
    words: &str,
    author: &str,
) -> anyhow::Result<Record> {
    if q.kind != Kind::Question {
        bail!("{} isn't a question", q.ref_id);
    }
    if q.state != "open" {
        bail!("{} is already {}", q.ref_id, q.state);
    }
    let options = q.data.get("options").and_then(Value::as_array).cloned().unwrap_or_default();
    let chosen = match option {
        Some(n) if n == 0 || n as usize > options.len() => bail!("there is no option {n}"),
        Some(n) => Some((n, options[n as usize - 1].get("title").and_then(Value::as_str).unwrap_or("").to_owned())),
        None => None,
    };
    if chosen.is_none() && words.trim().is_empty() {
        bail!("pick an option or write an answer");
    }
    let mut data = Map::new();
    data.insert("answer".into(), words.to_owned().into());
    data.insert("answered_at".into(), now_ms().into());
    if let Some((n, title)) = &chosen {
        data.insert("answer_option".into(), (*n).into());
        data.insert("answer_option_title".into(), title.clone().into());
    }
    let (q, _) = update(
        conn,
        q,
        author,
        Patch {
            state: Some("answered".into()),
            data,
            note: "answered by the owner".into(),
            quiet: true,
            ..Default::default()
        },
    )?;
    let what = match (&chosen, words.trim()) {
        (Some((n, t)), "") => format!("{n}: {t}"),
        (Some((n, t)), w) => format!("{n}: {t}. \"{w}\""),
        (None, w) => format!("\"{w}\""),
    };
    event(
        conn,
        Some(q.project_id),
        Some(q.id),
        "question.answered",
        &format!("Owner answered {}: {what}", q.short_id()),
        author,
    )?;
    Ok(q)
}

// ---------------------------------------------------------------------------------------------
// Links

/// Link relations and how they read from the other side.
pub const RELATIONS: &[(&str, &str, &str)] = &[
    // (relation, reads from `from`, reads from `to`)
    ("supersedes", "Replaces", "Replaced by"),
    ("answers", "Answers", "Answered by"),
    ("blocks", "Blocks", "Blocked by"),
    ("implements", "Carries out", "Carried out by"),
    ("records", "Records", "Recorded in"),
    ("depends_on", "Depends on", "Needed by"),
    ("feedback_on", "Feedback on", "Feedback"),
    ("relates", "Related", "Related"),
];

pub fn check_relation(relation: &str) -> anyhow::Result<()> {
    if !RELATIONS.iter().any(|r| r.0 == relation) {
        bail!("relation must be one of: {}", RELATIONS.iter().map(|r| r.0).collect::<Vec<_>>().join(", "));
    }
    Ok(())
}

/// Links two records (idempotent). A `supersedes` link marks the older decision or ADR replaced.
pub fn link(conn: &Connection, from: &Record, to: &Record, relation: &str, author: &str) -> anyhow::Result<bool> {
    check_relation(relation)?;
    if from.id == to.id {
        bail!("a record can't link to itself");
    }
    // These relations say something about (or change) the target, so they stay inside one project;
    // only `relates` and `depends_on` may point at another project's records.
    if from.project_id != to.project_id && !matches!(relation, "relates" | "depends_on") {
        bail!("\"{relation}\" links stay inside one project; {} belongs to {}", to.ref_id, to.project);
    }
    let n = conn.execute(
        "INSERT OR IGNORE INTO links (from_id, to_id, relation, author, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![from.id, to.id, relation, author, now_ms()],
    )?;
    if n == 0 {
        return Ok(false);
    }
    let note = format!("{} {}", relation.replace('_', " "), to.ref_id);
    update(conn, from, author, Patch { note: note.clone(), ..Default::default() })?;
    if relation == "supersedes" && matches!(to.kind, Kind::Decision | Kind::Adr) && to.state != "replaced" {
        update(
            conn,
            to,
            author,
            Patch {
                state: Some("replaced".into()),
                note: format!("replaced by {}", from.ref_id),
                ..Default::default()
            },
        )?;
    }
    Ok(true)
}

#[derive(Debug, Clone, Serialize)]
pub struct LinkView {
    /// How the link reads from this record, e.g. "Answers" or "Replaced by".
    pub label: String,
    pub relation: String,
    pub outgoing: bool,
    pub other: Record,
    /// The other record depends on this one but was last changed before it.
    pub stale: bool,
}

pub fn links_of(conn: &Connection, record: &Record) -> anyhow::Result<Vec<LinkView>> {
    let mut out = Vec::new();
    let mut stmt = conn.prepare("SELECT relation, to_id FROM links WHERE from_id = ?1 ORDER BY id")?;
    let rows: Vec<(String, i64)> =
        stmt.query_map([record.id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
    for (relation, id) in rows {
        let label = RELATIONS.iter().find(|r| r.0 == relation).map(|r| r.1).unwrap_or("Linked");
        out.push(LinkView { label: label.into(), relation, outgoing: true, other: get(conn, id)?, stale: false });
    }
    let mut stmt = conn.prepare("SELECT relation, from_id FROM links WHERE to_id = ?1 ORDER BY id")?;
    let rows: Vec<(String, i64)> =
        stmt.query_map([record.id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
    for (relation, id) in rows {
        let label = RELATIONS.iter().find(|r| r.0 == relation).map(|r| r.2).unwrap_or("Linked");
        let other = get(conn, id)?;
        let stale = relation == "depends_on" && other.updated_at < record.updated_at;
        out.push(LinkView { label: label.into(), relation, outgoing: false, other, stale });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Queries

#[derive(Debug, Default, Clone)]
pub struct Query {
    pub project_id: Option<i64>,
    pub kinds: Vec<Kind>,
    pub states: Vec<String>,
    pub milestone_ids: Vec<i64>,
    pub tag: Option<String>,
    pub text: Option<String>,
    pub since: Option<i64>,
    pub limit: i64,
}

pub fn query(conn: &Connection, q: &Query) -> anyhow::Result<Vec<Record>> {
    let mut sql = format!("{RECORD_SELECT} WHERE 1 = 1");
    let mut args: Vec<Value> = Vec::new();
    let mut push = |sql: &mut String, cond: &str, v: Value| {
        args.push(v);
        sql.push_str(&cond.replace('?', &format!("?{}", args.len())));
    };
    if let Some(p) = q.project_id {
        push(&mut sql, " AND r.project_id = ?", p.into());
    }
    if !q.kinds.is_empty() {
        let list = q.kinds.iter().map(|k| format!("'{}'", k.as_str())).collect::<Vec<_>>().join(",");
        sql.push_str(&format!(" AND r.kind IN ({list})"));
    }
    if !q.states.is_empty() {
        sql.push_str(" AND r.state IN (");
        for (i, s) in q.states.iter().enumerate() {
            push(&mut sql, if i == 0 { "?" } else { ",?" }, s.clone().into());
        }
        sql.push(')');
    }
    if !q.milestone_ids.is_empty() {
        let list = q.milestone_ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        sql.push_str(&format!(" AND r.milestone_id IN ({list})"));
    }
    if let Some(t) = &q.tag {
        push(&mut sql, " AND r.tags LIKE ?", format!("%,{},%", t.trim().to_lowercase()).into());
    }
    if let Some(t) = q.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        let like = format!("%{}%", t.replace('%', "\\%").replace('_', "\\_"));
        push(&mut sql, " AND (r.title LIKE ? ESCAPE '\\'", like.clone().into());
        push(&mut sql, " OR r.body LIKE ? ESCAPE '\\'", like.clone().into());
        push(&mut sql, " OR r.data LIKE ? ESCAPE '\\')", like.into());
    }
    if let Some(s) = q.since {
        push(&mut sql, " AND r.updated_at >= ?", s.into());
    }
    sql.push_str(" ORDER BY r.updated_at DESC");
    // 0 means everything (AgentFlow's own views); agents ask for a page.
    if q.limit > 0 {
        sql.push_str(&format!(" LIMIT {}", q.limit.min(1000)));
    }
    let params: Vec<rusqlite::types::Value> = args
        .into_iter()
        .map(|v| match v {
            Value::Number(n) => rusqlite::types::Value::Integer(n.as_i64().unwrap_or_default()),
            Value::String(s) => rusqlite::types::Value::Text(s),
            other => rusqlite::types::Value::Text(other.to_string()),
        })
        .collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params), record_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

// ---------------------------------------------------------------------------------------------
// Events

pub fn event(
    conn: &Connection,
    project_id: Option<i64>,
    record_id: Option<i64>,
    kind: &str,
    summary: &str,
    actor: &str,
) -> anyhow::Result<i64> {
    conn.execute(
        "INSERT INTO events (at, project_id, record_id, kind, summary, actor) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![now_ms(), project_id, record_id, kind, summary, actor],
    )?;
    Ok(conn.last_insert_rowid())
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub seq: i64,
    pub at: i64,
    pub project: Option<String>,
    pub record: Option<String>,
    pub kind: String,
    pub summary: String,
    pub actor: String,
}

/// Events in one project after `after_seq` that `actor` didn't cause, oldest first.
pub fn events_by_others(
    conn: &Connection,
    project_id: i64,
    actor: &str,
    after_seq: i64,
    limit: i64,
) -> anyhow::Result<Vec<Event>> {
    let mut stmt = conn.prepare(
        "SELECT e.seq, e.at, p.slug, r.kind, r.number, e.kind, e.summary, e.actor
         FROM events e LEFT JOIN projects p ON p.id = e.project_id LEFT JOIN records r ON r.id = e.record_id
         WHERE e.project_id = ?1 AND e.seq > ?2 AND e.actor != ?3 ORDER BY e.seq LIMIT ?4",
    )?;
    let rows = stmt.query_map(params![project_id, after_seq, actor, limit], event_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The newest events, newest first (the dashboard's activity, a project's history).
pub fn recent_events(conn: &Connection, project_id: Option<i64>, limit: i64) -> anyhow::Result<Vec<Event>> {
    let mut stmt = conn.prepare(
        "SELECT e.seq, e.at, p.slug, r.kind, r.number, e.kind, e.summary, e.actor
         FROM events e LEFT JOIN projects p ON p.id = e.project_id LEFT JOIN records r ON r.id = e.record_id
         WHERE (?1 IS NULL OR e.project_id = ?1) ORDER BY e.seq DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![project_id, limit], event_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn event_from_row(r: &Row) -> rusqlite::Result<Event> {
    let project: Option<String> = r.get(2)?;
    let kind: Option<String> = r.get(3)?;
    let number: Option<i64> = r.get(4)?;
    let record = match (&project, kind.as_deref().and_then(|k| Kind::parse(k).ok()), number) {
        (Some(p), Some(k), Some(n)) => Some(format!("{p}/{}-{n}", k.prefix())),
        _ => None,
    };
    Ok(Event { seq: r.get(0)?, at: r.get(1)?, project, record, kind: r.get(5)?, summary: r.get(6)?, actor: r.get(7)? })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn setup(c: &Connection) -> i64 {
        crate::projects::create(c, "vinculum", "Vinculum", "test").unwrap().id
    }

    #[test]
    fn refs_parse() {
        let r = RecordRef::parse("vinculum/Q-12").unwrap();
        assert_eq!((r.project.as_deref(), r.kind, r.number), (Some("vinculum"), Kind::Question, 12));
        let r = RecordRef::parse("t-3").unwrap();
        assert_eq!((r.project, r.kind, r.number), (None, Kind::Task, 3));
        assert_eq!(RecordRef::parse("vinculum/ADR-43").unwrap().kind, Kind::Adr);
        assert!(RecordRef::parse("Q12").is_err());
        assert!(RecordRef::parse("X-1").is_err());
    }

    #[test]
    fn create_update_revise() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let p = setup(c);
            let new = |title: &str| NewRecord { kind: Some(Kind::Task), title: title.into(), ..Default::default() };
            let t1 = create(c, p, "vinculum", new("Record a bind"))?;
            let t2 = create(c, p, "vinculum", new("Undo"))?;
            assert_eq!((t1.ref_id.as_str(), t2.number, t1.state.as_str()), ("vinculum/T-1", 2, "todo"));
            assert!(create(c, p, "x", new(" ")).is_err());

            let (t1b, changed) =
                update(c, &t1, "vinculum", Patch { state: Some("doing".into()), ..Default::default() })?;
            assert!(changed);
            assert_eq!(t1b.state, "doing");
            let (_, changed) =
                update(c, &t1b, "vinculum", Patch { state: Some("doing".into()), ..Default::default() })?;
            assert!(!changed, "no-op changes leave no revision");
            assert!(update(c, &t1b, "v", Patch { state: Some("answered".into()), ..Default::default() }).is_err());

            let revs = revisions(c, t1.id)?;
            assert_eq!(revs.len(), 2);
            assert_eq!(revs[0].changes["state"], "doing");

            let mut data = Map::new();
            data.insert("resurface_when".into(), "after M5".into());
            let (t, _) = update(
                c,
                &t1b,
                "v",
                Patch { data, tags: Some(vec!["Key Binds".into(), "ui".into()]), ..Default::default() },
            )?;
            assert_eq!(t.str("resurface_when"), "after M5");
            assert_eq!(t.tags, ["key-binds", "ui"]);
            let found = query(c, &Query { tag: Some("ui".into()), ..Default::default() })?;
            assert_eq!(found.len(), 1);
            let found = query(c, &Query { text: Some("undo".into()), kinds: vec![Kind::Task], ..Default::default() })?;
            assert_eq!(found[0].ref_id, "vinculum/T-2");
            let found = query(c, &Query { text: Some("100%".into()), ..Default::default() })?;
            assert!(found.is_empty());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn answers_are_verbatim() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let p = setup(c);
            let mut data = Map::new();
            data.insert("options".into(), json!([{"title": "A"}, {"title": "B"}]));
            let q = create(
                c,
                p,
                "v",
                NewRecord { kind: Some(Kind::Question), title: "A or B?".into(), data, ..Default::default() },
            )?;
            assert!(answer_question(c, &q, None, "  ", "owner").is_err());
            assert!(answer_question(c, &q, Some(3), "", "owner").is_err());
            let a = answer_question(c, &q, Some(2), "B, but  smaller ", "owner")?;
            assert_eq!(
                (a.state.as_str(), a.str("answer"), a.str("answer_option_title")),
                ("answered", "B, but  smaller ", "B")
            );
            assert!(answer_question(c, &a, Some(1), "", "owner").is_err(), "answered once");
            let ev = recent_events(c, Some(p), 1)?;
            assert!(ev[0].summary.contains("2: B"), "{}", ev[0].summary);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn links_that_change_their_target_stay_in_the_project() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let a = setup(c);
            let b = crate::projects::create(c, "other", "Other", "test")?.id;
            let dec = |p: i64| {
                create(c, p, "x", NewRecord { kind: Some(Kind::Decision), title: "d".into(), ..Default::default() })
            };
            let (mine, theirs) = (dec(a)?, dec(b)?);
            for relation in ["supersedes", "blocks", "implements", "answers", "records", "feedback_on"] {
                assert!(link(c, &mine, &theirs, relation, "vinculum").is_err(), "{relation}");
            }
            assert_eq!(get(c, theirs.id)?.state, "current", "untouched");
            assert!(link(c, &mine, &theirs, "relates", "vinculum")?);
            assert!(link(c, &mine, &theirs, "depends_on", "vinculum")?);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn links_supersede_and_staleness() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let p = setup(c);
            let dec = |t: &str| NewRecord { kind: Some(Kind::Decision), title: t.into(), ..Default::default() };
            let d1 = create(c, p, "v", dec("Integration branch is fine"))?;
            let task = create(
                c,
                p,
                "v",
                NewRecord { kind: Some(Kind::Task), title: "Release checklist".into(), ..Default::default() },
            )?;
            link(c, &task, &d1, "depends_on", "v")?;
            std::thread::sleep(std::time::Duration::from_millis(5));
            let d2 = create(c, p, "v", dec("Stack on PR bases"))?;
            assert!(link(c, &d2, &d1, "supersedes", "v")?);
            assert!(!link(c, &d2, &d1, "supersedes", "v")?, "idempotent");
            assert!(link(c, &d2, &d2, "relates", "v").is_err());
            assert!(link(c, &d2, &d1, "likes", "v").is_err());
            let d1 = get(c, d1.id)?;
            assert_eq!(d1.state, "replaced");
            let views = links_of(c, &d1)?;
            let replaced_by = views.iter().find(|l| l.relation == "supersedes").unwrap();
            assert_eq!(
                (replaced_by.label.as_str(), replaced_by.other.ref_id.as_str()),
                ("Replaced by", "vinculum/D-2")
            );
            // The task depends on D-1, which changed (was replaced) after the task did.
            assert!(views.iter().any(|l| l.relation == "depends_on" && l.stale));

            let evs = events_by_others(c, p, "nobody", 0, 100)?;
            assert!(evs.iter().any(|e| e.kind == "decision.updated" && e.summary.contains("now replaced")));
            assert_eq!(recent_events(c, None, 1)?[0].seq, evs.last().unwrap().seq);
            Ok(())
        })
        .unwrap();
    }
}
