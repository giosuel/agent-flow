//! What agents learn of the usage (the owner, 2026-11-17, M13: "I also want the agents to be able
//! to query the usage"): the plan's windows with their estimate, and per project its tokens and
//! their cost today, in the current 5-hour window, over 7 days, and its costliest tasks. The `usage`
//! MCP tool and `flow usage` both give this; the Usage page shows the same numbers.

use rusqlite::Connection;
use serde_json::{Value, json};

use crate::plan::{FIVE_HOURS, Past, WEEK};
use crate::usage::{self, Tokens};

/// How big 100% of a window is: dollars at API prices, new tokens, and what that rests on. The
/// median per 1% of finished windows used at least 10%; else this window so far, from 5% on.
pub fn estimate(past: &[(Past, Tokens)], so_far: &Tokens, pct: f64, five: bool) -> Option<(f64, f64, String)> {
    let ok: Vec<&(Past, Tokens)> = past.iter().filter(|(w, t)| w.peak >= 10.0 && t.calls > 0).collect();
    let usd = crate::plan::median(ok.iter().map(|(w, t)| t.usd() / w.peak).collect());
    let tok = crate::plan::median(ok.iter().map(|(w, t)| t.new_tokens() as f64 / w.peak).collect());
    match (usd, tok) {
        (Some(u), Some(t)) => Some((
            u * 100.0,
            t * 100.0,
            format!("median of {} window{}", ok.len(), if ok.len() == 1 { "" } else { "s" }),
        )),
        _ if pct >= 5.0 && so_far.calls > 0 => Some((
            so_far.usd() / pct * 100.0,
            so_far.new_tokens() as f64 / pct * 100.0,
            if five { "this window so far".into() } else { "this week so far".into() },
        )),
        _ => None,
    }
}

fn sum(rows: &[(Option<i64>, i64, Tokens)], project: Option<i64>) -> Tokens {
    rows.iter().filter(|(p, _, _)| project.is_none() || *p == project).fold(Tokens::default(), |mut a, (_, _, t)| {
        a.add(t);
        a
    })
}

fn tokens_json(t: &Tokens) -> Value {
    json!({
        "usd": (t.usd() * 100.0).round() / 100.0,
        "new_tokens": t.new_tokens(),
        "input": t.input,
        "output": t.output,
        "cache_writes": t.cache_write,
        "cache_reads": t.cache_read,
        "calls": t.calls,
        "calls_without_price": t.unpriced,
    })
}

fn rfc3339(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms).map(|t| t.to_string()).unwrap_or_default()
}

/// The plan's windows now, or why there are none.
/// The week's pace (D-295), as the Usage page draws it: ahead or behind an even pace, the rates,
/// and when it runs out at the last 24 hours' pace.
fn pace_json(p: &crate::plan::Pace) -> Value {
    let one = |v: f64| (v * 10.0).round() / 10.0;
    let mut v = json!({
        "week_gone": one(p.gone),
        "ahead_points": one(p.ahead()),
        "note": "ahead_points: % used less % of the week gone (negative: behind an even pace). runs_out_at: when it reaches 100% at the last 24 hours' pace, if that's before the reset; otherwise at_reset says where that pace ends.",
    });
    if let (Some(day), Some(week)) = (p.day_rate, p.week_rate) {
        v["per_day_last_24h"] = json!(one(day));
        v["per_day_this_week"] = json!(one(week));
        match p.runs_out(day) {
            Some(t) => v["runs_out_at"] = json!(rfc3339(t)),
            None => v["at_reset"] = json!(one(p.ends_at(day))),
        }
    }
    v
}

fn plan(c: &Connection, now: i64) -> anyhow::Result<Value> {
    let Some(s) = crate::plan::latest(c)? else {
        return Ok(
            json!({"note": "no status line has sent the plan's usage yet; `flow project setup` in a project's folder sets it up"}),
        );
    };
    let mut out = serde_json::Map::new();
    for (key, pct, reset, len, five) in
        [("five_hour", s.five_pct, s.five_reset, FIVE_HOURS, true), ("week", s.week_pct, s.week_reset, WEEK, false)]
    {
        let (Some(pct), Some(reset)) = (pct, reset) else { continue };
        if reset <= now {
            out.insert(
                key.into(),
                json!({"used": 0, "reset_at": rfc3339(reset), "note": "reset; no session has reported since"}),
            );
            continue;
        }
        let so_far = sum(&usage::per_project(c, reset - len, now)?, None);
        let mut past = Vec::new();
        for p in crate::plan::past(c, five, now, if five { 12 } else { 6 })? {
            past.push((p, sum(&usage::per_project(c, p.start, p.end)?, None)));
        }
        let mut w = json!({
            "used": pct,
            "resets_at": rfc3339(reset),
            "resets_in_min": (reset - now) / 60_000,
            "so_far": tokens_json(&so_far),
        });
        if let Some((usd, tok, basis)) = estimate(&past, &so_far, pct, five) {
            w["full_usd"] = json!(usd.round());
            w["full_new_tokens"] = json!(tok.round() as i64);
            w["estimate_from"] = json!(basis);
            w["left_usd"] = json!(((100.0 - pct).max(0.0) / 100.0 * usd).round());
        }
        if !five && let Some(p) = crate::plan::week_pace(c, now)? {
            w["pace"] = pace_json(&p);
        }
        out.insert(key.into(), w);
    }
    out.insert("seen".into(), json!(crate::util::ago(s.at, now)));
    out.insert(
        "note".into(),
        json!("The plan is the owner's account: all projects and Claude on their other devices share it. Estimates come from what AgentFlow counts."),
    );
    Ok(Value::Object(out))
}

/// The report: the plan, then `projects` (one, or every project with tokens when `project` is None).
pub fn report(c: &Connection, project: Option<i64>, now: i64, tz: &crate::tz::Tz) -> anyhow::Result<Value> {
    let days = tz.days(now, 7);
    let (today, week_from, end) = (days[6], days[0], days[7]);
    let five_from = crate::plan::latest(c)?.and_then(|s| s.five_reset).filter(|r| *r > now).map(|r| r - FIVE_HOURS);
    let today_rows = usage::per_project(c, today, end)?;
    let week_rows = usage::per_project(c, week_from, end)?;
    let window_rows = match five_from {
        Some(f) => usage::per_project(c, f, now)?,
        None => Vec::new(),
    };
    let tasks = usage::per_task(c, week_from, end)?;
    let projects = crate::projects::list(c)?;
    let mut ids: Vec<i64> = match project {
        Some(p) => vec![p],
        None => week_rows.iter().filter_map(|(p, _, _)| *p).collect(),
    };
    ids.sort();
    ids.dedup();
    let mut out = Vec::new();
    for id in ids {
        let Some(p) = projects.iter().find(|p| p.id == id) else { continue };
        let mut mine: Vec<(Option<i64>, Tokens)> =
            tasks.iter().filter(|t| t.project_id == Some(id)).map(|t| (t.task_id, t.total())).collect();
        mine.sort_by_key(|(_, t)| std::cmp::Reverse(t.cost_micro));
        let mut top = Vec::new();
        for (task, t) in mine.iter().take(10) {
            let (tid, title) = match task {
                Some(tid) => {
                    let r = crate::records::get(c, *tid)?;
                    (r.short_id(), r.title)
                }
                None => (String::new(), "not in a task".to_owned()),
            };
            top.push(json!({"id": tid, "title": title, "usd": (t.usd() * 100.0).round() / 100.0, "new_tokens": t.new_tokens()}));
        }
        let mut v = json!({
            "project": p.slug,
            "today": tokens_json(&sum(&today_rows, Some(id))),
            "last_7_days": tokens_json(&sum(&week_rows, Some(id))),
            "tasks_7_days": top,
        });
        if five_from.is_some() {
            v["this_5_hours"] = tokens_json(&sum(&window_rows, Some(id)));
        }
        out.push(v);
    }
    Ok(json!({
        "plan": plan(c, now)?,
        "projects": out,
        "all_projects_7_days": tokens_json(&sum(&week_rows, None)),
        "prices": "USD at Anthropic's API prices: input, output, cache reads and cache writes (5 min or 1 h) each at the model's price, fast mode doubled",
    }))
}

/// `text` cut to `n` characters, with "…" when it was longer.
fn clip(text: &str, n: usize) -> String {
    if text.chars().count() <= n {
        text.to_owned()
    } else {
        format!("{}…", text.chars().take(n.saturating_sub(1)).collect::<String>())
    }
}

/// `flow usage`, in flow's style (crate::ui): the plan, then each project, then its tasks.
pub fn print(v: &Value, all: bool) -> String {
    use crate::ui::{Tone, fit, paint};
    let on = crate::ui::color(false);
    let usd = |x: &Value| x.as_f64().map(crate::prices::dollars).unwrap_or_else(|| "–".into());
    let short = |x: &Value| crate::usage::short(x.as_i64().unwrap_or(0));
    let local = |s: &str| {
        s.parse::<jiff::Timestamp>()
            .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%a %H:%M").to_string())
            .unwrap_or_default()
    };
    let mut out = Vec::new();
    out.push(paint(Tone::Bold, "Plan", on));
    let plan = &v["plan"];
    let mut any = false;
    for (key, label) in [("five_hour", "5 hours"), ("week", "week")] {
        let w = &plan[key];
        if !w.is_object() {
            continue;
        }
        any = true;
        let used = w["used"].as_f64().unwrap_or(0.0);
        let pct = format!("{used:.0}%");
        let left = w["resets_in_min"].as_i64().map(|m| {
            if m >= 1440 { format!("in {} d", (m + 720) / 1440) } else { format!("in {} h {} min", m / 60, m % 60) }
        });
        let resets = match (w["resets_at"].as_str(), left) {
            (Some(r), Some(l)) => format!("resets {} ({l})", local(r)),
            _ => w["note"].as_str().unwrap_or("").to_owned(),
        };
        let full = match (w["full_usd"].as_f64(), w["full_new_tokens"].as_i64()) {
            (Some(u), Some(t)) => format!(
                "100% ≈ {} · {} new tokens ({})",
                crate::prices::dollars(u),
                crate::usage::short(t),
                w["estimate_from"].as_str().unwrap_or("")
            ),
            _ => String::new(),
        };
        out.push(fit(
            "  ",
            4,
            &[
                (Tone::Plain, &format!("{label:<8}")),
                (if used >= 80.0 { Tone::Commit } else { Tone::Number }, &format!("{pct:>4}")),
                (Tone::Dim, &resets),
                (Tone::Plain, &format!("so far {}", usd(&w["so_far"]["usd"]))),
                (Tone::Dim, &full),
            ],
            on,
        ));
        // The week against an even pace, and when it runs out at the last day's pace (D-295).
        let pace = &w["pace"];
        if pace.is_object() {
            let ahead = pace["ahead_points"].as_f64().unwrap_or(0.0).round();
            let side = if ahead >= 1.0 {
                format!("{ahead:.0} points ahead of")
            } else if ahead <= -1.0 {
                format!("{:.0} points behind", -ahead)
            } else {
                "on".to_owned()
            };
            let mut line =
                format!("{side} an even pace ({:.0}% of the week gone)", pace["week_gone"].as_f64().unwrap_or(0.0));
            let out_at = pace["runs_out_at"].as_str();
            if let Some(d) = pace["per_day_last_24h"].as_f64() {
                match out_at {
                    Some(r) => line += &format!(" · at {d}% a day it runs out {}", local(r)),
                    None => {
                        line += &format!(
                            " · at {d}% a day it lasts, ending near {:.0}%",
                            pace["at_reset"].as_f64().unwrap_or(0.0)
                        )
                    }
                }
            }
            out.push(crate::ui::wrap(4, if out_at.is_some() { Tone::Commit } else { Tone::Dim }, &line, on));
        }
    }
    if !any {
        out.push(crate::ui::wrap(2, Tone::Dim, plan["note"].as_str().unwrap_or("no samples yet"), on));
    }
    for p in v["projects"].as_array().into_iter().flatten() {
        out.push(String::new());
        out.push(paint(Tone::Bold, p["project"].as_str().unwrap_or(""), on));
        for (key, label) in [("today", "today"), ("this_5_hours", "this 5 hours"), ("last_7_days", "7 days")] {
            let t = &p[key];
            if !t.is_object() {
                continue;
            }
            out.push(fit(
                "  ",
                4,
                &[
                    (Tone::Plain, &format!("{label:<13}")),
                    (Tone::Number, &format!("{:>8}", usd(&t["usd"]))),
                    (
                        Tone::Dim,
                        &format!(
                            "{} new · {} cache reads · {} calls",
                            short(&t["new_tokens"]),
                            short(&t["cache_reads"]),
                            t["calls"].as_i64().unwrap_or(0)
                        ),
                    ),
                ],
                on,
            ));
        }
        let tasks: Vec<&Value> =
            p["tasks_7_days"].as_array().into_iter().flatten().take(if all { 3 } else { 8 }).collect();
        if !tasks.is_empty() {
            out.push(paint(Tone::Dim, "  tasks, 7 days", on));
            for t in tasks {
                out.push(fit(
                    "    ",
                    6,
                    &[
                        (Tone::Topic, &format!("{:<6}", t["id"].as_str().filter(|s| !s.is_empty()).unwrap_or("–"))),
                        (Tone::Number, &format!("{:>8}", usd(&t["usd"]))),
                        (Tone::Plain, &clip(t["title"].as_str().unwrap_or(""), crate::ui::width().saturating_sub(24))),
                    ],
                    on,
                ));
            }
        }
    }
    out.push(String::new());
    out.push(paint(Tone::Dim, "Dollars at API prices. flow usage --all for every project, --json for scripts.", on));
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_is_estimated_from_finished_ones_else_from_itself() {
        let t =
            |usd: f64, new: i64| Tokens { calls: 1, cost_micro: (usd * 1e6) as i64, input: new, ..Default::default() };
        let w = |peak: f64| Past { start: 0, end: 1, peak, limit_at: None };
        let past = vec![(w(50.0), t(125.0, 5_000_000)), (w(100.0), t(260.0, 9_000_000)), (w(4.0), t(1.0, 1))];
        let (usd, tok, basis) = estimate(&past, &Tokens::default(), 0.0, true).unwrap();
        assert_eq!((usd.round(), tok.round(), basis.as_str()), (255.0, 9_500_000.0, "median of 2 windows"));
        assert!(estimate(&[], &t(10.0, 100), 3.0, true).is_none(), "too early to say");
        let (usd, _, basis) = estimate(&[], &t(10.0, 100), 4.0 * 1.25, true).unwrap();
        assert_eq!((usd.round(), basis.as_str()), (200.0, "this window so far"));
    }

    #[test]
    fn flow_usage_says_the_weeks_pace() {
        let week = |pace: Value| json!({"plan": {"week": {"used": 64.0, "resets_at": "2026-12-02T01:24:00Z", "resets_in_min": 4000, "so_far": {"usd": 949.0}, "pace": pace}}, "projects": []});
        // Wrapped to the terminal: compared as one line.
        let flat = |t: String| t.split_whitespace().collect::<Vec<_>>().join(" ");
        let text = flat(print(
            &week(
                json!({"week_gone": 53.6, "ahead_points": 10.4, "per_day_last_24h": 19.0, "per_day_this_week": 17.1, "runs_out_at": "2026-11-30T17:11:00Z"}),
            ),
            false,
        ));
        assert!(
            text.contains("10 points ahead of an even pace (54% of the week gone) · at 19% a day it runs out "),
            "{text}"
        );
        let text = flat(print(
            &week(
                json!({"week_gone": 53.6, "ahead_points": -18.6, "per_day_last_24h": 6.3, "per_day_this_week": 9.3, "at_reset": 54.4}),
            ),
            false,
        ));
        assert!(
            text.contains(
                "19 points behind an even pace (54% of the week gone) · at 6.3% a day it lasts, ending near 54%"
            ),
            "{text}"
        );
        let text = flat(print(&week(json!({"week_gone": 1.0, "ahead_points": 0.2})), false));
        assert!(text.contains("on an even pace (1% of the week gone)") && !text.contains("a day"), "{text}");
    }
}
