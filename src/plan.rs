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

/// A window that just passed 80% (the owner, 2026-11-17: a push, once per window, D-195).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Crossed {
    pub five: bool,
    pub pct: f64,
    pub reset: i64,
}

/// What [`store`] did: whether it kept the sample, and the windows it saw pass 80%.
#[derive(Debug, Default, PartialEq)]
pub struct Stored {
    pub kept: bool,
    pub crossed: Vec<Crossed>,
}

/// Keeps a sample unless one came in the last 50 seconds.
pub fn store(conn: &Connection, l: &Limits, at: i64, project_id: Option<i64>, machine: &str) -> anyhow::Result<Stored> {
    let l = l.checked();
    if l.is_empty() {
        return Ok(Stored::default());
    }
    let recent: bool =
        conn.query_row("SELECT EXISTS (SELECT 1 FROM plan_samples WHERE at > ?1)", [at - 50_000], |r| r.get(0))?;
    if recent {
        return Ok(Stored::default());
    }
    // A window at 80% or more that no earlier sample saw there: once per window.
    let mut crossed = Vec::new();
    for (five, w, pct, reset) in
        [(true, l.five_hour, "five_pct", "five_reset"), (false, l.seven_day, "week_pct", "week_reset")]
    {
        let Some(w) = w.filter(|w| w.used_percentage >= 80.0) else { continue };
        let ms = w.resets_at * 1000;
        let seen: bool = conn.query_row(
            &format!("SELECT EXISTS (SELECT 1 FROM plan_samples WHERE {pct} >= 80 AND abs({reset} - ?1) < 120000)"),
            [ms],
            |r| r.get(0),
        )?;
        if !seen {
            crossed.push(Crossed { five, pct: w.used_percentage, reset: ms });
        }
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
    Ok(Stored { kept: true, crossed })
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

pub const DAY: i64 = 86_400_000;

/// The week's pace (the owner, 2026-11-27: "where we are in the weekly token budget compared where
/// we are in the week, if we are ahead or behind, and then also an indication when we will run out
/// of tokens if we continue at the current usage"; drawn as a burn-up chart, D-295).
#[derive(Debug, Clone, PartialEq)]
pub struct Pace {
    /// The week: from its start (its reset less seven days) to its reset.
    pub start: i64,
    pub reset: i64,
    pub now: i64,
    /// % used: the newest sample's, taken as still true now.
    pub used: f64,
    /// % of the week gone.
    pub gone: f64,
    /// % a day over the last 24 hours (since the week began when it's younger), and over the whole
    /// week so far; none in the week's first three hours.
    pub day_rate: Option<f64>,
    pub week_rate: Option<f64>,
    /// Whether `day_rate` covers a whole day.
    pub full_day: bool,
    /// The week's % used over time, oldest first: 0% at its start, the samples (thinned to one every
    /// 20 minutes), and `used` now.
    pub points: Vec<(i64, f64)>,
}

impl Pace {
    /// How many points of the budget more than an even pace has used (less: behind).
    pub fn ahead(&self) -> f64 {
        self.used - self.gone
    }

    /// When `rate` (% a day) from now reaches 100%, if that's before the reset; when it did, if it
    /// already has.
    pub fn runs_out(&self, rate: f64) -> Option<i64> {
        if self.used >= 100.0 {
            return self.points.iter().find(|(_, p)| *p >= 100.0).map(|(t, _)| *t);
        }
        if rate <= 0.0 {
            return None;
        }
        let at = self.now + ((100.0 - self.used) / rate * DAY as f64) as i64;
        (at < self.reset).then_some(at)
    }

    /// Where `rate` (% a day) from now ends at the reset, at most 100%.
    pub fn ends_at(&self, rate: f64) -> f64 {
        (self.used + rate.max(0.0) * (self.reset - self.now) as f64 / DAY as f64).min(100.0)
    }

    /// % used at `t`, between the points (the last one after it).
    pub fn used_at(&self, t: i64) -> f64 {
        let i = self.points.partition_point(|(at, _)| *at <= t);
        match (i.checked_sub(1).map(|k| self.points[k]), self.points.get(i).copied()) {
            (Some((a, pa)), Some((b, pb))) if b > a => pa + (pb - pa) * (t - a) as f64 / (b - a) as f64,
            (Some((_, p)), _) => p,
            (None, Some((_, p))) => p,
            (None, None) => 0.0,
        }
    }
}

/// The current week's pace, from its samples; none without a sample of a week still running.
pub fn week_pace(conn: &Connection, now: i64) -> anyhow::Result<Option<Pace>> {
    let Some(last) = latest(conn)? else { return Ok(None) };
    let (Some(used), Some(reset)) = (last.week_pct, last.week_reset) else { return Ok(None) };
    if reset <= now {
        return Ok(None);
    }
    let start = reset - WEEK;
    // This week's samples (a reset can come a second apart between samples), never going down.
    let mut points = vec![(start, 0.0)];
    let mut top = 0.0_f64;
    for s in between(conn, start, now + 1)? {
        let (Some(p), Some(r)) = (s.week_pct, s.week_reset) else { continue };
        if (r - reset).abs() >= 120_000 {
            continue;
        }
        top = top.max(p);
        if s.at - points[points.len() - 1].0 >= 20 * 60_000 {
            points.push((s.at, top));
        }
    }
    let used = used.max(top);
    match points.last_mut() {
        Some(last) if last.0 >= now => last.1 = used,
        _ => points.push((now, used)),
    }
    let mut pace = Pace {
        start,
        reset,
        now,
        used,
        gone: ((now - start) as f64 / WEEK as f64 * 100.0).clamp(0.0, 100.0),
        day_rate: None,
        week_rate: None,
        full_day: now - start >= DAY,
        points,
    };
    if now - start >= 3 * 3_600_000 {
        let from = (now - DAY).max(start);
        pace.day_rate = Some((used - pace.used_at(from)).max(0.0) / (now - from) as f64 * DAY as f64);
        pace.week_rate = Some(used / (now - start) as f64 * DAY as f64);
    }
    Ok(Some(pace))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn the_weeks_pace_compares_used_with_the_week_gone_and_says_when_it_runs_out() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let h = 3_600_000_i64;
            let reset = 1_764_600_000_000_i64; // a reset on the minute, in ms
            let start = reset - WEEK;
            let sample = |at: i64, pct: f64, r: i64| {
                let l =
                    Limits { five_hour: None, seven_day: Some(Window { used_percentage: pct, resets_at: r / 1000 }) };
                store(c, &l, at, None, "")
            };
            assert_eq!(week_pace(c, start + h)?, None, "no samples");
            // Last week's sample doesn't count; this week: 30% by day 3, then 20 points in the
            // last day (one sample every 10 minutes, kept one every 20).
            sample(start - h, 90.0, start)?;
            sample(start + 2 * DAY, 30.0, reset + 1000)?;
            let now = start + 3 * DAY + 12 * h;
            for k in 0..=24 * 6 {
                let at = now - DAY + k * 600_000;
                sample(at, 30.0 + 20.0 * (k as f64 / 144.0), reset)?;
            }
            let p = week_pace(c, now)?.unwrap();
            assert_eq!((p.start, p.reset), (start, reset));
            assert!((p.used - 50.0).abs() < 1e-9, "{}", p.used);
            assert!((p.gone - 50.0).abs() < 1e-9, "3.5 of 7 days: {}", p.gone);
            assert!(p.ahead().abs() < 1e-9);
            assert_eq!(p.points[0], (start, 0.0));
            assert!(p.points.windows(2).all(|w| w[1].0 - w[0].0 >= 20 * 60_000 || w[1].0 == now), "thinned");
            assert!(p.points.len() < 80, "{}", p.points.len());
            let day = p.day_rate.unwrap();
            assert!((day - 20.0).abs() < 0.2, "20 points in the last day: {day}");
            let week = p.week_rate.unwrap();
            assert!((week - 50.0 / 3.5).abs() < 1e-6, "{week}");
            assert!(p.full_day);
            // At 20% a day the other 50% take 2.5 days, before the reset in 3.5.
            let out = p.runs_out(day).unwrap();
            assert!((out - (now + 5 * DAY / 2)).abs() < 2 * h, "{}", (out - now) as f64 / DAY as f64);
            assert_eq!(p.ends_at(day), 100.0);
            // At 10% a day it lasts, and ends at 85%; at none it never runs out.
            assert_eq!(p.runs_out(10.0), None);
            assert!((p.ends_at(10.0) - 85.0).abs() < 1e-9);
            assert_eq!(p.runs_out(0.0), None);
            // Between two kept points, and after the last.
            assert!((p.used_at(start + DAY) - 15.0).abs() < 1e-9, "0% at the start, 30% two days in");
            assert_eq!(p.used_at(now + h), 50.0);
            // Used up: when it reached 100%.
            sample(now + h, 100.0, reset)?;
            let p = week_pace(c, now + 2 * h)?.unwrap();
            assert_eq!(p.runs_out(5.0), Some(now + h));
            assert_eq!(p.points.iter().filter(|(t, _)| *t >= now).count(), 3, "now, the 100% sample, and now again");
            Ok(())
        })
        .unwrap();
        // A young week has no rate yet; a week that's over has no pace.
        let young = Db::open_memory();
        young
            .call_blocking(|c| {
                let reset = 1_764_600_000_000_i64;
                let start = reset - WEEK;
                let l = Limits {
                    five_hour: None,
                    seven_day: Some(Window { used_percentage: 4.0, resets_at: reset / 1000 }),
                };
                store(c, &l, start + 3_600_000, None, "")?;
                let p = week_pace(c, start + 7_200_000)?.unwrap();
                assert_eq!((p.day_rate, p.week_rate, p.full_day), (None, None, false));
                assert_eq!((p.used, p.points.len()), (4.0, 3));
                assert_eq!(week_pace(c, reset + 1)?, None);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn samples_come_at_most_once_a_minute_and_nonsense_is_dropped() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let l: Limits = serde_json::from_str(
                r#"{"five_hour":{"used_percentage":42,"resets_at":1763419800},"seven_day":{"used_percentage":71.5,"resets_at":1763420400}}"#,
            )?;
            let first = store(c, &l, 1_000_000, None, "viber")?;
            assert!(first.kept);
            assert_eq!(first.crossed, vec![], "71% isn't past 80%");
            assert!(!store(c, &l, 1_030_000, None, "viber")?.kept, "within the minute");
            let odd = Limits { five_hour: Some(Window { used_percentage: -3.0, resets_at: 1763419800 }), seven_day: None };
            assert!(!store(c, &odd, 1_200_000, None, "viber")?.kept);
            assert!(store(c, &l, 1_100_000, None, "viber")?.kept);
            let s = latest(c)?.unwrap();
            assert_eq!((s.at, s.five_pct, s.five_reset, s.week_pct), (1_100_000, Some(42.0), Some(1_763_419_800_000), Some(71.5)));
            assert_eq!(between(c, 0, 2_000_000)?.len(), 2);
            // Windows that are over, newest first, with their peak.
            let w = |pct: f64, reset: i64| Limits { five_hour: Some(Window { used_percentage: pct, resets_at: reset }), seven_day: None };
            let (r1, r2) = (1_763_400_000_i64, 1_763_418_000_i64);
            for (k, (pct, reset)) in [(30.0, r1), (96.0, r1), (40.0, r2), (100.0, r2), (5.0, r2 + 18_000)].iter().enumerate() {
                let s = store(c, &w(*pct, *reset), 2_000_000 + k as i64 * 60_000, None, "")?;
                assert!(s.kept);
                // 96% crosses in the first window, 100% in the second; never twice in one.
                assert_eq!(!s.crossed.is_empty(), k == 1 || k == 3, "{k}: {s:?}");
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
