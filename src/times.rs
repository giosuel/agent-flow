//! Time spent (the owner, 2026-11-12: "we should also find a way to log times for all tasks").
//!
//! A task's time is the time it spent in doing, counted from its state changes: blocked, review
//! or todo stop the clock, and so does the agent being away (no call for 30 minutes), so a task
//! left in doing overnight doesn't count as work. Within a step, time when several tasks are in
//! doing at once is counted once for the step and split evenly among them (Imaginarium's agent:
//! weighting by estimate would hide the misses the comparison is there to show). Timings (wall,
//! CPU, peak memory of a build or test) are logged by `agentflow time`, `log_timing` and remote
//! builds, on a task, a step or a PR; they replace the timings in PR bodies.

use std::collections::HashMap;

use anyhow::bail;
use rusqlite::{Connection, params};
use serde::Serialize;
use serde_json::Value;

use crate::projects;
use crate::records::{self, Record};

/// No call from the agent for this long: it's away, and the clock stops. An hour, so waiting on a
/// subagent counts (Imaginarium's agent, 2026-11-12: "I often wait over 30 minutes").
pub const AWAY_AFTER: i64 = 60 * 60_000;

type Span = (i64, i64);

/// When the task was in doing, from its revisions.
pub fn doing_spans(conn: &Connection, task: &Record, now: i64) -> anyhow::Result<Vec<Span>> {
    let mut revs = records::revisions(conn, task.id)?;
    revs.sort_by_key(|r| r.rev);
    let mut spans = Vec::new();
    let mut since: Option<i64> = None;
    for r in &revs {
        let Some(state) = r.changes.get("state").and_then(Value::as_str) else { continue };
        match (state == "doing", since) {
            (true, None) => since = Some(r.at),
            (false, Some(s)) => {
                spans.push((s, r.at));
                since = None;
            }
            _ => {}
        }
    }
    if let Some(s) = since {
        spans.push((s, now));
    }
    Ok(spans)
}

/// When the project's agent was around: each of its calls counts for an hour, and a build or test
/// it ran (`agentflow time`, a build server job) for as long as it ran.
pub fn present_spans(conn: &Connection, slug: &str, from: i64, to: i64) -> anyhow::Result<Vec<Span>> {
    let mut stmt = conn.prepare(
        "SELECT at FROM log WHERE project = ?1 AND actor_kind = 'project' AND at >= ?2 AND at <= ?3 ORDER BY at",
    )?;
    let mut spans: Vec<Span> = stmt
        .query_map(params![slug, from - AWAY_AFTER, to], |r| r.get::<_, i64>(0))?
        .map(|r| r.map(|at| (at, at + AWAY_AFTER)))
        .collect::<Result<_, _>>()?;
    let mut stmt = conn.prepare(
        "SELECT t.at - t.wall_ms, t.at FROM timings t JOIN projects p ON p.id = t.project_id
         WHERE p.slug = ?1 AND t.at >= ?2 AND t.at - t.wall_ms <= ?3
         UNION ALL
         SELECT j.started_at, coalesce(j.finished_at, ?3) FROM remote_jobs j JOIN projects p ON p.id = j.project_id
         WHERE p.slug = ?1 AND j.started_at IS NOT NULL AND coalesce(j.finished_at, ?3) >= ?2 AND j.started_at <= ?3",
    )?;
    let runs: Vec<Span> = stmt.query_map(params![slug, from, to], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
    spans.extend(runs);
    spans.sort_unstable();
    let mut out: Vec<Span> = Vec::new();
    for (a, b) in spans {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    Ok(out)
}

fn intersect(a: &[Span], b: &[Span]) -> Vec<Span> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let s = a[i].0.max(b[j].0);
        let e = a[i].1.min(b[j].1);
        if s < e {
            out.push((s, e));
        }
        if a[i].1 < b[j].1 { i += 1 } else { j += 1 }
    }
    out
}

/// Times for a set of tasks worked on together (one step): its total (overlaps once) and each
/// task's share (overlaps split evenly), in milliseconds. A task's own correction replaces its
/// share, and the total moves with it.
#[derive(Debug, Default, Clone, Serialize)]
pub struct Times {
    pub total_ms: i64,
    pub share_ms: HashMap<i64, i64>,
    /// Raw time in doing per task, before splitting (for the task's own page).
    pub raw_ms: HashMap<i64, i64>,
}

pub fn times_of(conn: &Connection, slug: &str, tasks: &[&Record], now: i64) -> anyhow::Result<Times> {
    let mut per: Vec<(i64, Vec<Span>)> = Vec::new();
    let (mut lo, mut hi) = (i64::MAX, i64::MIN);
    for t in tasks {
        let spans = doing_spans(conn, t, now)?;
        if let (Some(f), Some(l)) = (spans.first(), spans.last()) {
            lo = lo.min(f.0);
            hi = hi.max(l.1);
        }
        per.push((t.id, spans));
    }
    let mut out = Times::default();
    if lo == i64::MAX {
        apply_overrides(tasks, &mut out);
        return Ok(out);
    }
    let present = present_spans(conn, slug, lo, hi)?;
    let per: Vec<(i64, Vec<Span>)> = per.into_iter().map(|(id, s)| (id, intersect(&s, &present))).collect();
    for (id, spans) in &per {
        out.raw_ms.insert(*id, spans.iter().map(|s| s.1 - s.0).sum());
    }
    // Sweep: between two change points, the tasks in doing share the time evenly.
    let mut points: Vec<i64> = per.iter().flat_map(|(_, s)| s.iter().flat_map(|x| [x.0, x.1])).collect();
    points.sort_unstable();
    points.dedup();
    let mut share: HashMap<i64, f64> = HashMap::new();
    for w in points.windows(2) {
        let (a, b) = (w[0], w[1]);
        let active: Vec<i64> =
            per.iter().filter(|(_, s)| s.iter().any(|x| x.0 <= a && x.1 >= b)).map(|(id, _)| *id).collect();
        if active.is_empty() {
            continue;
        }
        let len = (b - a) as f64;
        out.total_ms += b - a;
        for id in &active {
            *share.entry(*id).or_default() += len / active.len() as f64;
        }
    }
    out.share_ms = share.into_iter().map(|(k, v)| (k, v.round() as i64)).collect();
    apply_overrides(tasks, &mut out);
    Ok(out)
}

fn apply_overrides(tasks: &[&Record], out: &mut Times) {
    for t in tasks {
        if let Some(min) = t.data.get("time_override_min").and_then(Value::as_i64) {
            let old = out.share_ms.get(&t.id).copied().unwrap_or(0);
            out.total_ms += min * 60_000 - old;
            out.share_ms.insert(t.id, min * 60_000);
        }
    }
}

/// The times of one step (or a milestone's own tasks).
pub fn step_times(conn: &Connection, step_id: i64, now: i64) -> anyhow::Result<(Times, i64)> {
    let m = projects::get_milestone(conn, step_id)?;
    let p = projects::get_by_id(conn, m.project_id)?;
    let tasks = crate::steps::tasks_of(conn, step_id)?;
    let counted: Vec<&Record> = tasks.iter().filter(|t| t.state != "skipped").collect();
    let est: i64 = counted.iter().map(|t| estimate_min(t)).sum();
    Ok((times_of(conn, &p.slug, &counted, now)?, est))
}

/// A task's estimate in minutes (older tasks only have the text).
pub fn estimate_min(t: &Record) -> i64 {
    t.data.get("estimate_min").and_then(Value::as_i64).or_else(|| crate::steps::parse_minutes(t.str("estimate"))).unwrap_or(0)
}

/// "1 h 50 min", "35 min", "—".
pub fn fmt_ms(ms: i64) -> String {
    let min = (ms as f64 / 60_000.0).round() as i64;
    match min {
        0 if ms <= 0 => "—".into(),
        0 => "under 1 min".into(),
        m if m < 60 => format!("{m} min"),
        m if m % 60 == 0 => format!("{} h", m / 60),
        m => format!("{} h {} min", m / 60, m % 60),
    }
}

pub fn fmt_min(min: i64) -> String {
    fmt_ms(min * 60_000)
}

// ---------------------------------------------------------------------------------------------
// Timings

#[derive(Debug, Clone, Serialize)]
pub struct Timing {
    pub id: i64,
    pub what: String,
    pub place: String,
    pub wall_ms: i64,
    pub cpu_ms: Option<i64>,
    pub peak_mb: Option<i64>,
    pub exit_code: Option<i64>,
    pub note: String,
    pub at: i64,
    pub on: String,
}

pub struct NewTiming<'a> {
    pub what: &'a str,
    pub place: &'a str,
    pub wall_ms: i64,
    pub cpu_ms: Option<i64>,
    pub peak_mb: Option<i64>,
    pub exit_code: Option<i64>,
    pub note: &'a str,
}

/// What a timing is on: a task, a step or a PR. Left out: the one task in doing, or else the one
/// step under way.
pub enum On {
    Record(Record),
    Step(projects::Milestone),
}

pub fn resolve_on(conn: &Connection, project_id: i64, slug: &str, on: Option<&str>) -> anyhow::Result<On> {
    if let Some(on) = on.map(str::trim).filter(|o| !o.is_empty()) {
        if let Ok(r) = records::RecordRef::parse(on)
            && let Some(rec) = records::try_find(conn, &r, Some(slug))?
        {
            if rec.project_id != project_id {
                bail!("{} belongs to another project", rec.ref_id);
            }
            return Ok(On::Record(rec));
        }
        let on = on.trim_start_matches('#');
        if let Ok(n) = on.parse::<i64>()
            && let Some(rec) = records::try_find(
                conn,
                &records::RecordRef { project: Some(slug.into()), kind: records::Kind::Pr, number: n },
                None,
            )?
        {
            return Ok(On::Record(rec));
        }
        return Ok(On::Step(projects::find_milestone(conn, project_id, on)?));
    }
    let doing = records::query(
        conn,
        &records::Query {
            project_id: Some(project_id),
            kinds: vec![records::Kind::Task],
            states: vec!["doing".into()],
            ..Default::default()
        },
    )?;
    if doing.len() == 1 {
        return Ok(On::Record(doing.into_iter().next().unwrap()));
    }
    let active: Vec<projects::Milestone> = projects::milestones(conn, project_id)?
        .into_iter()
        .filter(|m| m.state == "active" && m.parent_id.is_some())
        .collect();
    if active.len() == 1 {
        return Ok(On::Step(active.into_iter().next().unwrap()));
    }
    bail!("say what it's on (--on): a task (T-12), a step (P9.3d) or a PR (#58)")
}

pub fn add(conn: &Connection, project_id: i64, on: &On, t: &NewTiming, actor: &str) -> anyhow::Result<Timing> {
    let what = t.what.trim();
    if what.is_empty() || what.chars().count() > 200 {
        bail!("say what was timed in a line (up to 200 characters), e.g. \"cargo test --workspace\"");
    }
    if t.wall_ms < 0 || t.wall_ms > 7 * 86_400_000 {
        bail!("wall time is out of range");
    }
    let (rid, mid, label) = match on {
        On::Record(r) => (Some(r.id), None, records::display_id(r)),
        On::Step(m) => (None, Some(m.id), m.name.clone()),
    };
    let now = crate::util::now_ms();
    conn.execute(
        "INSERT INTO timings (project_id, record_id, milestone_id, what, place, wall_ms, cpu_ms, peak_mb, exit_code, note, actor, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            project_id,
            rid,
            mid,
            crate::activity::redact(what),
            t.place.trim(),
            t.wall_ms,
            t.cpu_ms,
            t.peak_mb,
            t.exit_code,
            crate::activity::redact(t.note.trim()),
            actor,
            now
        ],
    )?;
    let id = conn.last_insert_rowid();
    Ok(Timing {
        id,
        what: what.into(),
        place: t.place.trim().into(),
        wall_ms: t.wall_ms,
        cpu_ms: t.cpu_ms,
        peak_mb: t.peak_mb,
        exit_code: t.exit_code,
        note: t.note.trim().into(),
        at: now,
        on: label,
    })
}

fn timing_from_row(r: &rusqlite::Row) -> rusqlite::Result<Timing> {
    Ok(Timing {
        id: r.get(0)?,
        what: r.get(1)?,
        place: r.get(2)?,
        wall_ms: r.get(3)?,
        cpu_ms: r.get(4)?,
        peak_mb: r.get(5)?,
        exit_code: r.get(6)?,
        note: r.get(7)?,
        at: r.get(8)?,
        on: r.get(9)?,
    })
}

const TIMING_SELECT: &str = "SELECT t.id, t.what, t.place, t.wall_ms, t.cpu_ms, t.peak_mb, t.exit_code, t.note, t.at,
    coalesce(CASE WHEN r.kind = 'pr' THEN '#' || r.number ELSE upper(substr(r.kind, 1, 1)) || '-' || r.number END, m.name, '')
    FROM timings t LEFT JOIN records r ON r.id = t.record_id LEFT JOIN milestones m ON m.id = t.milestone_id";

/// Timings on one record (a task, a PR), newest first.
pub fn of_record(conn: &Connection, record_id: i64) -> anyhow::Result<Vec<Timing>> {
    let mut stmt = conn.prepare(&format!("{TIMING_SELECT} WHERE t.record_id = ?1 ORDER BY t.at DESC LIMIT 50"))?;
    let rows = stmt.query_map([record_id], timing_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Timings of a step: on it, on its tasks, and on the PRs that carry its tasks out.
pub fn of_step(conn: &Connection, step_id: i64) -> anyhow::Result<Vec<Timing>> {
    let mut stmt = conn.prepare(&format!(
        "{TIMING_SELECT} WHERE t.milestone_id = ?1
            OR t.record_id IN (SELECT id FROM records WHERE kind = 'task' AND milestone_id = ?1)
            OR t.record_id IN (SELECT l.from_id FROM links l JOIN records x ON x.id = l.to_id
                               WHERE l.relation = 'implements' AND x.kind = 'task' AND x.milestone_id = ?1)
         ORDER BY t.at DESC LIMIT 100"
    ))?;
    let rows = stmt.query_map([step_id], timing_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// A timing as a page shows it.
pub struct TimingRow {
    pub what: String,
    pub place: String,
    pub wall: String,
    pub cpu: String,
    pub peak: String,
    pub when: String,
    pub on: String,
    pub failed: bool,
}

pub fn rows(list: &[Timing], tz: &crate::tz::Tz) -> Vec<TimingRow> {
    list.iter()
        .map(|t| TimingRow {
            what: t.what.clone(),
            place: t.place.clone(),
            wall: fmt_wall(t.wall_ms),
            cpu: t.cpu_ms.map(|c| format!("{:.0} s", c as f64 / 1000.0)).unwrap_or_default(),
            peak: t.peak_mb.filter(|m| *m > 0).map(fmt_mb).unwrap_or_default(),
            when: tz.short(t.at),
            on: t.on.clone(),
            failed: t.exit_code.is_some_and(|c| c != 0),
        })
        .collect()
}

/// "2 min 41 s", "8.6 s".
pub fn fmt_wall(ms: i64) -> String {
    let s = ms as f64 / 1000.0;
    if s < 60.0 {
        format!("{s:.1} s")
    } else if s < 3600.0 {
        format!("{} min {} s", (s / 60.0) as i64, (s % 60.0) as i64)
    } else {
        format!("{} h {} min", (s / 3600.0) as i64, ((s % 3600.0) / 60.0) as i64)
    }
}

pub fn fmt_mb(mb: i64) -> String {
    if mb >= 1024 { format!("{:.1} GB", mb as f64 / 1024.0) } else { format!("{mb} MB") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::records::{Kind, NewRecord};

    #[test]
    fn overlaps_count_once_and_away_stops_the_clock() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let p = crate::projects::create(c, "lumen", "Lumen", "owner")?;
            let new = |t: &str| records::create(c, p.id, "lumen", NewRecord { kind: Some(Kind::Task), title: t.into(), ..Default::default() });
            let (a, b) = (new("A")?, new("B")?);
            let min = 60_000;
            // A in doing 0–60 min, B 30–90 min (minutes from t0).
            let t0 = 1_000_000_000_000;
            let set = |id: i64, rev: i64, at: i64, state: &str| {
                c.execute(
                    "INSERT INTO revisions (record_id, rev, at, author, changes, note) VALUES (?1, ?2, ?3, 'lumen', ?4, '')",
                    params![id, rev, at, serde_json::json!({"state": state}).to_string()],
                )
            };
            set(a.id, 2, t0, "doing")?;
            set(a.id, 3, t0 + 60 * min, "done")?;
            set(b.id, 2, t0 + 30 * min, "doing")?;
            set(b.id, 3, t0 + 90 * min, "done")?;
            // The agent calls every 10 minutes from 0 to 50, then not until 80.
            for m in [0, 10, 20, 30, 40, 50, 80] {
                c.execute(
                    "INSERT INTO log (at, actor_kind, actor, project, via, action, summary) VALUES (?1, 'project', 'lumen', 'lumen', 'mcp', 'write', 'x')",
                    [t0 + m * min],
                )?;
            }
            let (a, b) = (records::get(c, a.id)?, records::get(c, b.id)?);
            let t = times_of(c, "lumen", &[&a, &b], t0 + 200 * min)?;
            // Present 0–140: all of 0–90 counts. A alone 0–30, both 30–60, B alone 60–90.
            assert_eq!(t.total_ms, 90 * min);
            assert_eq!(t.share_ms[&a.id], 45 * min);
            assert_eq!(t.share_ms[&b.id], 45 * min);
            // Only the calls up to 10: present until 70, so 70–90 no longer counts…
            c.execute("DELETE FROM log WHERE at > ?1", [t0 + 10 * min])?;
            let t = times_of(c, "lumen", &[&a, &b], t0 + 200 * min)?;
            assert_eq!(t.total_ms, 70 * min);
            assert_eq!(t.raw_ms[&b.id], 40 * min);
            // …unless a test it timed ran then.
            c.execute(
                "INSERT INTO timings (project_id, what, wall_ms, actor, at) VALUES (?1, 'cargo test', ?2, 'lumen', ?3)",
                params![p.id, 25 * min, t0 + 95 * min],
            )?;
            let t = times_of(c, "lumen", &[&a, &b], t0 + 200 * min)?;
            assert_eq!(t.total_ms, 90 * min);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn spans_meet_and_format() {
        assert_eq!(intersect(&[(0, 100), (200, 300)], &[(50, 250)]), vec![(50, 100), (200, 250)]);
        assert_eq!(fmt_ms(110 * 60_000), "1 h 50 min");
        assert_eq!(fmt_ms(120 * 60_000), "2 h");
        assert_eq!(fmt_ms(35 * 60_000), "35 min");
        assert_eq!(fmt_ms(0), "—");
        assert_eq!(fmt_wall(161_000), "2 min 41 s");
        assert_eq!(fmt_mb(10_035), "9.8 GB");
    }
}
