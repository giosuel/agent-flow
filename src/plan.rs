//! The plan's usage limits (the owner, 2026-11-17, M13: "is there a way to programmatically get
//! the usage in % from claude?"): Claude Code gives its status line the account's 5-hour and weekly
//! windows (`rate_limits`: how much is used, when each resets). `flow statusline` sends them here
//! at most once a minute; the limits are the account's, so any session's sample stands for all.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// One window as Claude Code reports it: % used, and when it resets (Unix seconds).
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
pub struct Window {
    pub used_percentage: f64,
    pub resets_at: i64,
}

/// Claude Code's `rate_limits`: either window may be missing (not reported, or already reset).
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq)]
pub struct Limits {
    #[serde(default)]
    pub five_hour: Option<Window>,
    #[serde(default)]
    pub seven_day: Option<Window>,
}

impl Limits {
    fn sane(w: Option<Window>) -> Option<Window> {
        w.filter(|w| {
            (0.0..=1000.0).contains(&w.used_percentage) && w.resets_at > 1_600_000_000 && w.resets_at < 4_000_000_000
        })
    }

    pub fn checked(self) -> Self {
        Self { five_hour: Self::sane(self.five_hour), seven_day: Self::sane(self.seven_day) }
    }

    pub fn is_empty(&self) -> bool {
        self.five_hour.is_none() && self.seven_day.is_none()
    }
}

/// A stored sample: when, and the two windows (resets in milliseconds).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Sample {
    pub at: i64,
    pub five_pct: Option<f64>,
    pub five_reset: Option<i64>,
    pub week_pct: Option<f64>,
    pub week_reset: Option<i64>,
}

/// The window lengths, for where a window started.
pub const FIVE_HOURS: i64 = 5 * 3_600_000;
pub const WEEK: i64 = 7 * 86_400_000;

/// Keeps a sample unless one came in the last 50 seconds; returns whether it was kept.
pub fn store(conn: &Connection, l: &Limits, at: i64, project_id: Option<i64>, machine: &str) -> anyhow::Result<bool> {
    let l = l.checked();
    if l.is_empty() {
        return Ok(false);
    }
    let recent: bool =
        conn.query_row("SELECT EXISTS (SELECT 1 FROM plan_samples WHERE at > ?1)", [at - 50_000], |r| r.get(0))?;
    if recent {
        return Ok(false);
    }
    conn.execute(
        "INSERT INTO plan_samples (at, five_pct, five_reset, week_pct, week_reset, project_id, machine)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            at,
            l.five_hour.map(|w| w.used_percentage),
            l.five_hour.map(|w| w.resets_at * 1000),
            l.seven_day.map(|w| w.used_percentage),
            l.seven_day.map(|w| w.resets_at * 1000),
            project_id,
            machine.chars().take(64).collect::<String>(),
        ],
    )?;
    Ok(true)
}

fn sample(r: &rusqlite::Row) -> rusqlite::Result<Sample> {
    Ok(Sample { at: r.get(0)?, five_pct: r.get(1)?, five_reset: r.get(2)?, week_pct: r.get(3)?, week_reset: r.get(4)? })
}

/// The newest sample.
pub fn latest(conn: &Connection) -> anyhow::Result<Option<Sample>> {
    Ok(conn
        .query_row(
            "SELECT at, five_pct, five_reset, week_pct, week_reset FROM plan_samples ORDER BY at DESC LIMIT 1",
            [],
            sample,
        )
        .optional()?)
}

/// The samples between two times, oldest first.
pub fn between(conn: &Connection, from: i64, to: i64) -> anyhow::Result<Vec<Sample>> {
    let mut stmt = conn.prepare(
        "SELECT at, five_pct, five_reset, week_pct, week_reset FROM plan_samples WHERE at >= ?1 AND at < ?2 ORDER BY at",
    )?;
    let rows = stmt.query_map(params![from, to], sample)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// A window that's over: when it ran and how much of it was used at most; `limit_at` when it
/// reached 100%.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Past {
    pub start: i64,
    pub end: i64,
    pub peak: f64,
    pub limit_at: Option<i64>,
}

/// The last `n` windows that ended before `now`, newest first: the 5-hour ones, or the weeks.
pub fn past(conn: &Connection, five: bool, now: i64, n: usize) -> anyhow::Result<Vec<Past>> {
    let (pct, reset, len) =
        if five { ("five_pct", "five_reset", FIVE_HOURS) } else { ("week_pct", "week_reset", WEEK) };
    // Resets can come a second apart between samples of one window: grouped by the minute.
    let mut stmt = conn.prepare(&format!(
        "SELECT {reset} / 60000 * 60000 AS r, max({pct}), min(CASE WHEN {pct} >= 100 THEN at END)
         FROM plan_samples WHERE {reset} IS NOT NULL AND {pct} IS NOT NULL
         GROUP BY r HAVING r <= ?1 ORDER BY r DESC LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![now, n as i64], |r| {
        let end: i64 = r.get(0)?;
        Ok(Past { start: end - len, end, peak: r.get(1)?, limit_at: r.get(2)? })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The middle value (the mean of the two middle ones for an even count).
pub fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let m = v.len() / 2;
    Some(if !v.len().is_multiple_of(2) { v[m] } else { (v[m - 1] + v[m]) / 2.0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn samples_come_at_most_once_a_minute_and_nonsense_is_dropped() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let l: Limits = serde_json::from_str(
                r#"{"five_hour":{"used_percentage":42,"resets_at":1763419800},"seven_day":{"used_percentage":71.5,"resets_at":1763420400}}"#,
            )?;
            assert!(store(c, &l, 1_000_000, None, "viber")?);
            assert!(!store(c, &l, 1_030_000, None, "viber")?, "within the minute");
            let odd = Limits { five_hour: Some(Window { used_percentage: -3.0, resets_at: 1763419800 }), seven_day: None };
            assert!(!store(c, &odd, 1_200_000, None, "viber")?);
            assert!(store(c, &l, 1_100_000, None, "viber")?);
            let s = latest(c)?.unwrap();
            assert_eq!((s.at, s.five_pct, s.five_reset, s.week_pct), (1_100_000, Some(42.0), Some(1_763_419_800_000), Some(71.5)));
            assert_eq!(between(c, 0, 2_000_000)?.len(), 2);
            // Windows that are over, newest first, with their peak.
            let w = |pct: f64, reset: i64| Limits { five_hour: Some(Window { used_percentage: pct, resets_at: reset }), seven_day: None };
            let (r1, r2) = (1_763_400_000_i64, 1_763_418_000_i64);
            for (k, (pct, reset)) in [(30.0, r1), (96.0, r1), (40.0, r2), (100.0, r2), (5.0, r2 + 18_000)].iter().enumerate() {
                assert!(store(c, &w(*pct, *reset), 2_000_000 + k as i64 * 60_000, None, "")?);
            }
            let p = past(c, true, r2 * 1000 + 1, 10)?;
            assert_eq!(p.len(), 2, "the one still running isn't over");
            assert_eq!((p[0].end, p[0].peak, p[0].start), (r2 * 1000, 100.0, r2 * 1000 - FIVE_HOURS));
            assert!(p[0].limit_at.is_some() && p[1].limit_at.is_none());
            assert_eq!(median(vec![3.0, 1.0, 2.0, 10.0]), Some(2.5));
            Ok(())
        })
        .unwrap();
    }
}
