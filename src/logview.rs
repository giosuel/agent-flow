//! Reading the activity log: a small query language, facet counts, a timeline, pages of entries,
//! and the archive of old reads.
//!
//! The owner, 2026-11-11: reads are kept 90 days, "and then compressed archive, don't delete
//! anything ever completely". Changes, messages and everything else stay in the database for
//! good; reads older than 90 days move to one zstd-compressed JSON-lines file per month, which a
//! search reaches when its time range does.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use rusqlite::{Connection, Row, params_from_iter};
use serde::{Deserialize, Serialize};

pub const READS_KEPT_DAYS: i64 = 90;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogRow {
    pub seq: i64,
    pub at: i64,
    pub actor_kind: String,
    pub actor: String,
    pub project: Option<String>,
    pub via: String,
    pub action: String,
    pub summary: String,
    pub target: Option<String>,
    pub status: Option<i64>,
    pub duration_ms: Option<i64>,
    pub ip: Option<String>,
    pub detail: Option<String>,
    pub severity: String,
}

const COLUMNS: &str =
    "seq, at, actor_kind, actor, project, via, action, summary, target, status, duration_ms, ip, detail, severity";

fn from_row(r: &Row) -> rusqlite::Result<LogRow> {
    Ok(LogRow {
        seq: r.get(0)?,
        at: r.get(1)?,
        actor_kind: r.get(2)?,
        actor: r.get(3)?,
        project: r.get(4)?,
        via: r.get(5)?,
        action: r.get(6)?,
        summary: r.get(7)?,
        target: r.get(8)?,
        status: r.get(9)?,
        duration_ms: r.get(10)?,
        ip: r.get(11)?,
        detail: r.get(12)?,
        severity: r.get(13)?,
    })
}

/// Fields a query can name, and their columns.
const FIELDS: &[(&str, &str)] = &[
    ("severity", "severity"),
    ("project", "project"),
    ("actor", "actor"),
    ("kind", "actor_kind"),
    ("action", "action"),
    ("via", "via"),
    ("target", "target"),
    ("ip", "ip"),
];

/// A parsed query: `severity:warn project:vinculum -action:read merge failed`.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Filter {
    /// (column, value, negated)
    pub fields: Vec<(String, String, bool)>,
    /// Words that must all appear in the summary, actor or target.
    pub words: Vec<String>,
    pub from: Option<i64>,
    pub to: Option<i64>,
}

impl Filter {
    pub fn parse(q: &str) -> Filter {
        let mut f = Filter::default();
        for token in q.split_whitespace() {
            let (neg, t) = match token.strip_prefix('-') {
                Some(rest) if rest.contains(':') => (true, rest),
                _ => (false, token),
            };
            match t.split_once(':') {
                Some((k, v)) if !v.is_empty() && FIELDS.iter().any(|(name, _)| name.eq_ignore_ascii_case(k)) => {
                    let col =
                        FIELDS.iter().find(|(name, _)| name.eq_ignore_ascii_case(k)).map(|(_, c)| *c).unwrap_or("");
                    let v = v.trim_matches('"').to_owned();
                    // "warning" and "warn" mean the same.
                    let v = if col == "severity" && v.eq_ignore_ascii_case("warning") { "warn".into() } else { v };
                    f.fields.push((col.to_owned(), v.to_lowercase(), neg));
                }
                _ => f.words.push(token.to_lowercase()),
            }
        }
        f
    }

    fn sql(&self) -> (String, Vec<rusqlite::types::Value>) {
        use rusqlite::types::Value as V;
        let mut sql = String::from(" WHERE 1 = 1");
        let mut args: Vec<V> = Vec::new();
        for (col, v, neg) in &self.fields {
            args.push(V::Text(v.clone()));
            // Columns come from FIELDS only, never from the query text.
            sql.push_str(&format!(" AND lower(coalesce({col}, '')) {} ?{}", if *neg { "!=" } else { "=" }, args.len()));
        }
        for w in &self.words {
            args.push(V::Text(format!("%{}%", w.replace('%', "\\%").replace('_', "\\_"))));
            let n = args.len();
            sql.push_str(&format!(
                " AND (lower(summary) LIKE ?{n} ESCAPE '\\' OR lower(actor) LIKE ?{n} ESCAPE '\\' OR lower(coalesce(target, '')) LIKE ?{n} ESCAPE '\\')"
            ));
        }
        if let Some(from) = self.from {
            args.push(V::Integer(from));
            sql.push_str(&format!(" AND at >= ?{}", args.len()));
        }
        if let Some(to) = self.to {
            args.push(V::Integer(to));
            sql.push_str(&format!(" AND at < ?{}", args.len()));
        }
        (sql, args)
    }

    /// The same test in Rust, for archived rows.
    pub fn matches(&self, r: &LogRow) -> bool {
        let value = |col: &str| -> String {
            match col {
                "severity" => r.severity.clone(),
                "project" => r.project.clone().unwrap_or_default(),
                "actor" => r.actor.clone(),
                "actor_kind" => r.actor_kind.clone(),
                "action" => r.action.clone(),
                "via" => r.via.clone(),
                "target" => r.target.clone().unwrap_or_default(),
                "ip" => r.ip.clone().unwrap_or_default(),
                _ => String::new(),
            }
            .to_lowercase()
        };
        self.fields.iter().all(|(c, v, neg)| (value(c) == *v) != *neg)
            && self.words.iter().all(|w| {
                r.summary.to_lowercase().contains(w)
                    || r.actor.to_lowercase().contains(w)
                    || r.target.as_deref().unwrap_or("").to_lowercase().contains(w)
            })
            && self.from.is_none_or(|f| r.at >= f)
            && self.to.is_none_or(|t| r.at < t)
    }
}

/// A page of entries, newest first, older than `before` (a seq) when given.
pub fn page(conn: &Connection, f: &Filter, before: Option<i64>, limit: i64) -> anyhow::Result<Vec<LogRow>> {
    let (mut sql, mut args) = f.sql();
    if let Some(b) = before {
        args.push(rusqlite::types::Value::Integer(b));
        sql.push_str(&format!(" AND seq < ?{}", args.len()));
    }
    let mut stmt =
        conn.prepare(&format!("SELECT {COLUMNS} FROM log{sql} ORDER BY seq DESC LIMIT {}", limit.clamp(1, 500)))?;
    let rows = stmt.query_map(params_from_iter(args), from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Entries after `after` (a seq), oldest first: the live stream.
pub fn newer(conn: &Connection, f: &Filter, after: i64) -> anyhow::Result<Vec<LogRow>> {
    let (mut sql, mut args) = f.sql();
    args.push(rusqlite::types::Value::Integer(after));
    sql.push_str(&format!(" AND seq > ?{}", args.len()));
    let mut stmt = conn.prepare(&format!("SELECT {COLUMNS} FROM log{sql} ORDER BY seq LIMIT 200"))?;
    let rows = stmt.query_map(params_from_iter(args), from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn latest_seq(conn: &Connection) -> anyhow::Result<i64> {
    Ok(conn.query_row("SELECT coalesce(max(seq), 0) FROM log", [], |r| r.get(0))?)
}

/// How many matching entries have each value of `column` (the sidebar).
pub fn facet(conn: &Connection, f: &Filter, column: &str) -> anyhow::Result<Vec<(String, i64)>> {
    let col = FIELDS.iter().find(|(_, c)| *c == column).map(|(_, c)| *c).context("unknown field")?;
    let (sql, args) = f.sql();
    let mut stmt = conn
        .prepare(&format!("SELECT coalesce({col}, ''), count(*) FROM log{sql} GROUP BY 1 ORDER BY 2 DESC LIMIT 12"))?;
    let rows = stmt.query_map(params_from_iter(args), |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Counts in `buckets` equal slices of [from, to) (the timeline).
pub fn histogram(conn: &Connection, f: &Filter, from: i64, to: i64, buckets: i64) -> anyhow::Result<Vec<i64>> {
    let width = ((to - from) / buckets).max(1);
    let mut f = f.clone();
    f.from = Some(from);
    f.to = Some(to);
    let (sql, mut args) = f.sql();
    args.push(rusqlite::types::Value::Integer(from));
    args.push(rusqlite::types::Value::Integer(width));
    let n = args.len();
    let mut stmt = conn.prepare(&format!("SELECT (at - ?{}) / ?{n}, count(*) FROM log{sql} GROUP BY 1", n - 1))?;
    let mut out = vec![0; buckets as usize];
    let rows = stmt.query_map(params_from_iter(args), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
    for row in rows {
        let (i, c) = row?;
        if let Some(slot) = out.get_mut(i as usize) {
            *slot += c;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// The archive

pub fn archive_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("log-archive")
}

/// `2026-08` for a time.
fn month_of(ms: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    format!("{:04}-{:02}", t.year(), t.month() as u8)
}

/// Moves reads older than 90 days into the month's archive file, then removes them from the
/// database. Each run appends one zstd frame; readers handle any number of frames. Returns how
/// many rows moved.
pub fn archive_old_reads(conn: &mut Connection, data_dir: &Path, now: i64) -> anyhow::Result<usize> {
    let cutoff = now - READS_KEPT_DAYS * 86_400_000;
    let rows: Vec<LogRow> = {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM log WHERE action = 'read' AND at < ?1 ORDER BY seq LIMIT 50000"
        ))?;
        let rows = stmt.query_map([cutoff], from_row)?;
        rows.collect::<Result<_, _>>()?
    };
    if rows.is_empty() {
        return Ok(0);
    }
    let dir = archive_dir(data_dir);
    std::fs::create_dir_all(&dir)?;
    let mut by_month: std::collections::BTreeMap<String, Vec<&LogRow>> = Default::default();
    for r in &rows {
        by_month.entry(month_of(r.at)).or_default().push(r);
    }
    for (month, list) in &by_month {
        let path = dir.join(format!("{month}-reads.jsonl.zst"));
        let file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        let mut enc = zstd::Encoder::new(file, 9)?;
        for r in list {
            serde_json::to_writer(&mut enc, r)?;
            enc.write_all(b"\n")?;
        }
        let file = enc.finish()?;
        // On disk before the rows leave the database.
        file.sync_all()?;
    }
    let tx = conn.transaction()?;
    let last = rows.last().map(|r| r.seq).unwrap_or(0);
    tx.execute("DELETE FROM log WHERE action = 'read' AND at < ?1 AND seq <= ?2", [cutoff, last])?;
    tx.commit()?;
    Ok(rows.len())
}

/// The archive's months, with how many reads each holds (for the page's footer).
pub fn archive_months(data_dir: &Path) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(archive_dir(data_dir)) else { return out };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(month) = name.strip_suffix("-reads.jsonl.zst") {
            let lines = read_archive(&e.path(), &Filter::default(), usize::MAX).map(|r| r.len() as u64).unwrap_or(0);
            out.push((month.to_owned(), lines));
        }
    }
    out.sort();
    out.reverse();
    out
}

/// Matching rows from one archive file, newest first, at most `limit`.
pub fn read_archive(path: &Path, f: &Filter, limit: usize) -> anyhow::Result<Vec<LogRow>> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(zstd::Decoder::new(file)?);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if let Ok(r) = serde_json::from_str::<LogRow>(&line)
            && f.matches(&r)
        {
            out.push(r);
        }
    }
    out.sort_by_key(|r| std::cmp::Reverse(r.seq));
    out.truncate(limit);
    Ok(out)
}

/// Archived rows matching the filter, for months the time range reaches, newest first.
pub fn search_archive(data_dir: &Path, f: &Filter, before: Option<i64>, limit: usize) -> anyhow::Result<Vec<LogRow>> {
    let mut out = Vec::new();
    for (month, _) in archive_months(data_dir) {
        // Skip months entirely outside the range.
        let starts =
            time::Date::parse(&format!("{month}-01"), time::macros::format_description!("[year]-[month]-[day]"))
                .map(|d| d.midnight().assume_utc().unix_timestamp() * 1000)
                .unwrap_or(0);
        if f.to.is_some_and(|to| starts >= to) || f.from.is_some_and(|from| starts + 32 * 86_400_000 < from) {
            continue;
        }
        let path = archive_dir(data_dir).join(format!("{month}-reads.jsonl.zst"));
        out.extend(read_archive(&path, f, usize::MAX)?.into_iter().filter(|r| before.is_none_or(|b| r.seq < b)));
        if out.len() >= limit {
            break;
        }
    }
    out.sort_by_key(|r| std::cmp::Reverse(r.seq));
    out.truncate(limit);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::{Action, Entry, Via, write_conn};
    use crate::auth::Actor;
    use crate::db::Db;

    #[test]
    fn queries_parse() {
        let f = Filter::parse("severity:warning -action:read Project:vinculum merge failed");
        assert_eq!(
            f.fields,
            vec![
                ("severity".into(), "warn".into(), false),
                ("action".into(), "read".into(), true),
                ("project".into(), "vinculum".into(), false)
            ]
        );
        assert_eq!(f.words, vec!["merge", "failed"]);
        // Unknown fields are words, so nothing reaches SQL as a column.
        let f = Filter::parse("seq:1;drop");
        assert!(f.fields.is_empty() && f.words == vec!["seq:1;drop"]);
    }

    #[test]
    fn pages_facets_and_the_archive() {
        let db = Db::open_memory();
        let dir = tempfile::tempdir().unwrap();
        db.call_blocking(|c| {
            let owner = Actor::Owner { session_id: 1, method: "passkey".into() };
            for i in 0..30 {
                let e = Entry::new(
                    &owner,
                    Via::Page,
                    if i % 3 == 0 { Action::Write } else { Action::Read },
                    format!("GET /p/{i}"),
                );
                write_conn(c, &e)?;
            }
            let f = Filter::parse("-action:read");
            assert_eq!(page(c, &f, None, 100)?.len(), 10);
            let facets = facet(c, &Filter::default(), "action")?;
            assert_eq!(facets[0], ("read".into(), 20));
            let first = page(c, &Filter::default(), None, 5)?;
            let older = page(c, &Filter::default(), Some(first.last().unwrap().seq), 5)?;
            assert!(older[0].seq < first[4].seq);
            let now = crate::util::now_ms();
            let h = histogram(c, &Filter::default(), now - 3_600_000, now + 1, 12)?;
            assert_eq!(h.iter().sum::<i64>(), 30);

            // Pretend the reads are old: they move to the archive, nothing else does, and a
            // search still finds them.
            c.execute("UPDATE log SET at = at - 100 * 86400000 WHERE action = 'read'", [])?;
            let moved = archive_old_reads(c, dir.path(), now)?;
            assert_eq!(moved, 20);
            assert_eq!(page(c, &Filter::default(), None, 100)?.len(), 10, "writes stay");
            assert_eq!(archive_old_reads(c, dir.path(), now)?, 0);
            let found = search_archive(dir.path(), &Filter::parse("/p/4"), None, 50)?;
            assert_eq!(found.len(), 1);
            assert_eq!(archive_months(dir.path()).iter().map(|(_, n)| n).sum::<u64>(), 20);
            // A second run appends another frame; both are read.
            write_conn(c, &Entry::new(&owner, Via::Page, Action::Read, "GET /late"))?;
            c.execute("UPDATE log SET at = at - 100 * 86400000 WHERE summary = 'GET /late'", [])?;
            archive_old_reads(c, dir.path(), now)?;
            assert_eq!(search_archive(dir.path(), &Filter::default(), None, 100)?.len(), 21);
            Ok(())
        })
        .unwrap();
    }
}
