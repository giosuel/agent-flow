//! The Usage page: token counts per day (an hour per bar when a day is opened), one line per
//! agent, a table per project, and the tasks that cost most, all of them in a dialog
//! (`/usage/tasks`).

use askama::Template;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Extension, Router};
use serde::Deserialize;

use super::charts::{self, Series};
use super::pages::{Nav, nav, render};
use super::{AppResult, AppState};
use crate::auth::Actor;
use crate::records::{self, Kind};
use crate::tz::Tz;
use crate::usage::{self, Tokens, short};
use crate::util::now_ms;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/usage", get(page))
        .route("/usage/tasks", get(all_tasks))
        .route("/transcripts/read", axum::routing::post(read_now))
}

#[derive(Deserialize, Default)]
struct UsageQuery {
    /// "tasks": the old Tasks tab, now `/usage/tasks`.
    tab: Option<String>,
    /// "2026-11-10": that day's hours instead of 14 days.
    day: Option<String>,
    /// What the chart counts: "total", "cache" (reads), "cost"; new tokens otherwise (the
    /// owner, 2026-11-17: "lets make \"new tokens\" the default view, and add cost and \"total
    /// tokens\" as other options").
    metric: Option<String>,
    /// The tasks' window: 7 days, or 14.
    days: Option<i64>,
}

/// The tasks' window: the last 7 days unless 14 are asked for.
fn task_days(q: Option<i64>) -> i64 {
    if q == Some(14) { 14 } else { 7 }
}

struct Legend {
    name: String,
    color: &'static str,
    total: String,
}

struct ProjectRow {
    name: String,
    color: &'static str,
    sessions: i64,
    t: Cells,
}

/// A row's numbers, formatted.
struct Cells {
    calls: String,
    input: String,
    cache_write: String,
    output: String,
    new_tokens: String,
    cache_read: String,
    /// At API prices; "–" when none of it has a price.
    cost: String,
    /// The numbers, to sort by.
    raw: Tokens,
}

impl Cells {
    fn of(t: &Tokens) -> Self {
        Self {
            calls: crate::util::fmt_count(t.calls),
            input: short(t.input),
            cache_write: short(t.cache_write),
            output: short(t.output),
            new_tokens: short(t.new_tokens()),
            cache_read: short(t.cache_read),
            cost: cost_of(t),
            raw: *t,
        }
    }
}

fn cost_of(t: &Tokens) -> String {
    if t.cost_micro == 0 && t.unpriced > 0 { "–".into() } else { crate::prices::dollars(t.usd()) }
}

/// What the chart counts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Metric {
    New,
    Total,
    Cache,
    Cost,
}

impl Metric {
    fn parse(s: Option<&str>) -> Self {
        match s {
            Some("total") => Metric::Total,
            Some("cache") => Metric::Cache,
            Some("cost") => Metric::Cost,
            _ => Metric::New,
        }
    }

    fn of(self, t: &Tokens) -> f64 {
        match self {
            Metric::New => t.new_tokens() as f64,
            Metric::Total => t.total() as f64,
            Metric::Cache => t.cache_read as f64,
            Metric::Cost => t.usd(),
        }
    }

    fn fmt(self, v: f64) -> String {
        match self {
            Metric::Cost => crate::prices::dollars(v),
            _ => short(v as i64),
        }
    }

    fn key(self) -> &'static str {
        match self {
            Metric::New => "",
            Metric::Total => "total",
            Metric::Cache => "cache",
            Metric::Cost => "cost",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Metric::New => "New tokens",
            Metric::Total => "Total tokens",
            Metric::Cache => "Cache reads",
            Metric::Cost => "Cost",
        }
    }
}

/// One of the plan's windows, as a meter (M13, the "Usage: plan and cost" mockups).
struct Meter {
    label: &'static str,
    pct: String,
    width: f64,
    warn: bool,
    reset: String,
    so_far: String,
    so_far_sub: String,
    /// "100% ≈ $250 at API prices ≈ 10.3M new tokens", and what it's based on.
    estimate: String,
    basis: String,
}

/// A finished 5-hour window in "How big a window is".
struct EstRow {
    /// Raw values to sort by: when it started, % used, cost, cost per 1%, new tokens per 1%.
    v: (i64, f64, f64, f64, f64),
    day: String,
    span: String,
    used: String,
    limit: String,
    cost: String,
    per_pct: String,
    tokens_per_pct: String,
}

struct TaskRow {
    /// Counted against a task, not "Not in a task".
    in_task: bool,
    usd: f64,
    /// The last call counted against it.
    last_at: i64,
    /// Its cost against the most any row costs, in % (the bar beside the cost).
    bar: u32,
    id: String,
    url: String,
    title: String,
    project: String,
    color: &'static str,
    status: String,
    worked: String,
    estimate: String,
    t: Cells,
    main: String,
    subagents: String,
}

struct Tile {
    label: String,
    value: String,
    hint: String,
}

#[derive(Template)]
#[template(path = "usage.html")]
struct UsagePage {
    nav: Nav,
    /// The transcripts are being read right now.
    reading: bool,
    newest: String,
    tiles: Vec<Tile>,
    chart_title: String,
    chart_sub: String,
    bars: String,
    bars_max: String,
    x_labels: Vec<String>,
    zoomed: bool,
    /// Query string that keeps the day while switching the metric.
    day_q: String,
    legend: Vec<Legend>,
    lines: String,
    lines_max: String,
    projects: Vec<ProjectRow>,
    range: String,
    /// The tasks that cost most in the window (the card), and what the card doesn't show.
    top_tasks: Vec<TaskRow>,
    tasks_total: usize,
    tasks_more: usize,
    not_in_task: String,
    days: i64,
    /// Keeps the opened day and the metric while switching the tasks' window, and the tasks'
    /// window while switching the metric.
    keep_day_metric: String,
    keep_days: String,
    empty: bool,
    /// The chart's metric, as the switch shows it: (key, name, on).
    metrics: Vec<(&'static str, &'static str, bool)>,
    /// The plan's windows, when a status line has sent them; why not otherwise.
    meters: Vec<Meter>,
    plan_seen: String,
    /// The plan's % under the bars: its title and chart, empty without samples.
    strip: String,
    strip_title: String,
    est_rows: Vec<EstRow>,
    est_median: (String, String),
    /// Calls of models without a price in this window, not in the cost.
    unpriced: i64,
}

async fn page(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<UsageQuery>,
    tz: Tz,
) -> AppResult<Response> {
    actor.require_owner()?;
    let now = now_ms();
    // The owner's local days (their browser's time zone): the last 14, today last.
    let fortnight_days = tz.days(now, 14);
    let today = fortnight_days[13];
    let end = fortnight_days[14];
    if q.tab.as_deref() == Some("tasks") {
        let days = q.days.map(|d| format!("?days={}", task_days(Some(d)))).unwrap_or_default();
        return Ok(Redirect::to(&format!("/usage/tasks{days}")).into_response());
    }
    let metric_kind = Metric::parse(q.metric.as_deref());
    let day = q.day.as_deref().and_then(|s| tz.parse_day(s)).filter(|d| *d <= today);
    let days = task_days(q.days);
    // A bar per local hour of an opened day (23 or 25 when the clocks change), else per day.
    let bounds = match day {
        Some(d) => tz.hours(d),
        None => fortnight_days.clone(),
    };
    let n = bounds.len() - 1;
    let fortnight_from = fortnight_days[0];
    let bounds_q = bounds.clone();
    let data = state
        .db
        .call(move |c| {
            let projects = crate::projects::list(c)?;
            let buckets = usage::buckets(c, &bounds_q)?;
            let per_project = usage::per_project(c, bounds_q[0], bounds_q[bounds_q.len() - 1])?;
            let today_total = usage::per_project(c, today, end)?;
            let fortnight = usage::per_project(c, fortnight_from, end)?;
            let newest: Option<i64> = c.query_row("SELECT max(at) FROM usage", [], |r| r.get(0))?;
            let week = usage::per_project(c, fortnight_days[7], end)?;
            let plan = plan_data(c, now, bounds_q[0], bounds_q[bounds_q.len() - 1])?;
            Ok((projects, buckets, per_project, today_total, fortnight, newest, week, plan))
        })
        .await?;
    let (projects, buckets, per_project, today_total, fortnight, newest, week, plan) = data;

    let slugs: Vec<String> = projects.iter().map(|p| p.slug.clone()).collect();
    let colors = charts::colors(&slugs);
    let name_of = |id: Option<i64>| -> (String, &'static str) {
        match id.and_then(|id| projects.iter().position(|p| p.id == id)) {
            Some(i) => (projects[i].name.clone(), colors[i]),
            None => ("Other".into(), charts::OTHER),
        }
    };
    let metric = |t: &Tokens| metric_kind.of(t);

    // One series per project that has any tokens in the window, in project order; Other last.
    let mut ids: Vec<Option<i64>> = projects.iter().map(|p| Some(p.id)).collect();
    ids.push(None);
    let mut series = Vec::new();
    let mut legend = Vec::new();
    let mut ids_of: Vec<(String, Option<i64>)> = Vec::new();
    for id in ids {
        let mut values = vec![0.0; n];
        for (b, p, t) in &buckets {
            if *p == id
                && let Some(v) = values.get_mut(*b)
            {
                *v += metric(t);
            }
        }
        let total: f64 = values.iter().sum();
        if total <= 0.0 {
            continue;
        }
        let (name, color) = name_of(id);
        legend.push(Legend { name: name.clone(), color, total: metric_kind.fmt(total) });
        ids_of.push((name.clone(), id));
        series.push(Series { name, color, values });
    }
    let labels: Vec<String> =
        bounds[..n].iter().map(|t| if day.is_some() { tz.hm(*t) } else { tz.day_label(*t) }).collect();
    let metric_q = match metric_kind.key() {
        "" => String::new(),
        k => format!("&metric={k}"),
    };
    let link_tz = tz.clone();
    let link_bounds = bounds.clone();
    let link = move |i: usize| format!("/usage?day={}{metric_q}", link_tz.date(link_bounds[i]));
    let fmt = |v: f64| metric_kind.fmt(v);
    let what = metric_kind.name().to_lowercase();
    let (bars, bars_max) = charts::stacked_bars(
        &format!("{what} per {}", if day.is_some() { "hour" } else { "day" }),
        &labels,
        &series,
        &fmt,
        if day.is_none() { Some(&link) } else { None },
    );
    // The lines show what the bars don't: how many model calls each project's agent made.
    let call_series: Vec<Series> = series
        .iter()
        .map(|s| {
            let id = ids_of.iter().find(|(name, _)| *name == s.name).map(|(_, id)| *id).unwrap_or(None);
            let mut values = vec![0.0; n];
            for (b, p, t) in &buckets {
                if *p == id
                    && let Some(v) = values.get_mut(*b)
                {
                    *v += t.calls as f64;
                }
            }
            Series { name: s.name.clone(), color: s.color, values }
        })
        .collect();
    let calls_fmt = |v: f64| format!("{} calls", crate::util::fmt_count(v as i64));
    let (lines, lines_max) = charts::lines("model calls per project", &labels, &call_series, &calls_fmt);
    let x_labels: Vec<String> = labels
        .iter()
        .enumerate()
        .map(|(i, l)| if day.is_some() && i % 3 != 0 { String::new() } else { l.clone() })
        .collect();

    let sum = |rows: &[(Option<i64>, i64, Tokens)]| {
        rows.iter().fold(Tokens::default(), |mut a, (_, _, t)| {
            a.add(t);
            a
        })
    };
    let (td, fn_, window) = (sum(&today_total), sum(&fortnight), sum(&per_project));
    let calls = |t: &Tokens| format!("{} calls", crate::util::fmt_count(t.calls));
    let ratio = |t: &Tokens| {
        if t.new_tokens() > 0 {
            format!("{:.0}× the new tokens", t.cache_read as f64 / t.new_tokens() as f64)
        } else {
            String::new()
        }
    };
    // An opened day gets its own figures; the 14-day view shows today and the fortnight.
    let wk = sum(&week);
    let tokens_hint =
        |t: &Tokens| format!("{} new tokens · {} cache reads", short(t.new_tokens()), short(t.cache_read));
    let tiles = match day {
        Some(d) => vec![
            Tile { label: format!("Cost on {}", tz.date(d)), value: cost_of(&window), hint: tokens_hint(&window) },
            Tile { label: "Calls that day".into(), value: crate::util::fmt_count(window.calls), hint: ratio(&window) },
        ],
        None => vec![
            Tile { label: "Today".into(), value: cost_of(&td), hint: tokens_hint(&td) },
            Tile { label: "Last 7 days".into(), value: cost_of(&wk), hint: calls(&wk) },
            Tile {
                label: "Last 14 days".into(),
                value: cost_of(&fn_),
                hint: format!("≈ {} a day", crate::prices::dollars(fn_.usd() / 14.0)),
            },
            Tile { label: "Cache reads, 14 days".into(), value: short(fn_.cache_read), hint: ratio(&fn_) },
        ],
    };

    let mut project_rows: Vec<ProjectRow> = per_project
        .iter()
        .map(|(id, sessions, t)| {
            let (name, color) = name_of(*id);
            ProjectRow { name, color, sessions: *sessions, t: Cells::of(t) }
        })
        .collect();
    project_rows.sort_by(|a, b| (a.name == "Other").cmp(&(b.name == "Other")).then(a.name.cmp(&b.name)));

    // The card: the six tasks that cost most; "Not in a task" only as a sum under them.
    let (in_task, outside): (Vec<TaskRow>, Vec<TaskRow>) =
        task_rows(&state, &tz, now, days).await?.into_iter().partition(|t| t.in_task);
    let tasks_total = in_task.len();
    let mut top_tasks: Vec<TaskRow> = in_task.into_iter().take(6).collect();
    set_bars(&mut top_tasks);
    let not_in_task =
        if outside.is_empty() { String::new() } else { crate::prices::dollars(outside.iter().map(|t| t.usd).sum()) };

    let day_q = day.map(|d| format!("day={}&", tz.date(d))).unwrap_or_default();
    let keep_day_metric = format!(
        "{day_q}{}",
        match metric_kind.key() {
            "" => String::new(),
            k => format!("metric={k}&"),
        }
    );
    let (meters, est_rows, est_median) = plan_view(&plan, now, &tz);
    render(UsagePage {
        reading: super::READING.load(std::sync::atomic::Ordering::Relaxed),
        nav: nav(&state, &actor, "/usage").await,
        newest: newest.map(|t| crate::util::ago(t, now)).unwrap_or_else(|| "not yet".into()),
        tiles,
        chart_title: match day {
            Some(d) => format!("{} by hour · {}", metric_kind.name(), tz.date(d)),
            None => format!("{} per day · 14 days", metric_kind.name()),
        },
        chart_sub: if day.is_some() { format!("Hours in {}", tz.name()) } else { "Open a day for its hours".into() },
        bars,
        bars_max: metric_kind.fmt(bars_max),
        x_labels,
        zoomed: day.is_some(),
        day_q,
        empty: legend.is_empty(),
        legend,
        lines,
        lines_max: crate::util::fmt_count(lines_max as i64),
        projects: project_rows,
        range: match day {
            Some(d) => tz.date(d),
            None => "14 days".into(),
        },
        tasks_more: tasks_total.saturating_sub(top_tasks.len()),
        tasks_total,
        not_in_task,
        top_tasks,
        days,
        keep_day_metric,
        keep_days: if days == 14 { "days=14&".into() } else { String::new() },
        metrics: [Metric::New, Metric::Total, Metric::Cache, Metric::Cost]
            .iter()
            .map(|m| (m.key(), m.name(), *m == metric_kind))
            .collect(),
        plan_seen: plan.latest.map(|s| crate::util::ago(s.at, now)).unwrap_or_default(),
        strip_title: if day.is_some() {
            "5 hours · % used".into()
        } else {
            "Week · % used at the end of the day".into()
        },
        strip: if plan.samples.is_empty() {
            String::new()
        } else {
            charts::percent_line("the plan's % used", &labels, &strip_values(&plan.samples, &bounds, day.is_some()))
        },
        meters,
        est_rows,
        est_median,
        unpriced: window.unpriced,
    })
}

/// Every task with tokens in the last `days` days, the costliest first, then what was "Not in a
/// task" (one row per project), with their bars set.
async fn task_rows(state: &AppState, tz: &Tz, now: i64, days: i64) -> AppResult<Vec<TaskRow>> {
    let bounds = tz.days(now, days as usize);
    let (from, to) = (bounds[0], bounds[bounds.len() - 1]);
    let (projects, tasks, records) = state
        .db
        .call(move |c| {
            let projects = crate::projects::list(c)?;
            let tasks = usage::per_task(c, from, to)?;
            let mut records = Vec::new();
            for t in &tasks {
                records.push(match t.task_id {
                    Some(id) => Some(records::get(c, id)?),
                    None => None,
                });
            }
            Ok((projects, tasks, records))
        })
        .await?;
    let slugs: Vec<String> = projects.iter().map(|p| p.slug.clone()).collect();
    let colors = charts::colors(&slugs);
    let mut rows: Vec<TaskRow> = tasks
        .iter()
        .zip(&records)
        .map(|(t, r)| {
            let (project, color) = match t.project_id.and_then(|id| projects.iter().position(|p| p.id == id)) {
                Some(i) => (projects[i].name.clone(), colors[i]),
                None => ("Other".into(), charts::OTHER),
            };
            let worked = if tz.date(t.first_at) == tz.date(t.last_at) {
                format!("{} {}–{}", tz.date(t.first_at), tz.hm(t.first_at), tz.hm(t.last_at))
            } else {
                format!("{} – {}", tz.date(t.first_at), tz.date(t.last_at))
            };
            let total = t.total();
            TaskRow {
                in_task: r.is_some(),
                usd: total.usd(),
                last_at: t.last_at,
                bar: 0,
                // With the project: every project has its own T-7.
                id: r.as_ref().map(|r| format!("{}/{}", r.project, r.short_id())).unwrap_or_default(),
                url: r.as_ref().map(|r| r.url()).unwrap_or_default(),
                title: r.as_ref().map(|r| r.title.clone()).unwrap_or_else(|| "Not in a task".into()),
                project,
                color,
                status: r.as_ref().map(|r| r.state.clone()).unwrap_or_default(),
                worked,
                estimate: r
                    .as_ref()
                    .filter(|r| r.kind == Kind::Task)
                    .map(|r| r.str("estimate").to_owned())
                    .unwrap_or_default(),
                t: Cells::of(&total),
                main: format!("{} new · {} calls", short(t.main.new_tokens()), t.main.calls),
                subagents: format!("{} new · {} calls", short(t.subagents.new_tokens()), t.subagents.calls),
            }
        })
        .collect();
    // Tasks first, by cost (the owner, 2026-11-23, Q-5: "Tasks by cost"); "Not in a task" last.
    rows.sort_by(|a, b| b.in_task.cmp(&a.in_task).then(b.usd.total_cmp(&a.usd)));
    set_bars(&mut rows);
    Ok(rows)
}

/// Each task's bar: its cost against the costliest task's, at least a sliver when it cost
/// anything. "Not in a task" gets none: it would dwarf the tasks.
fn set_bars(rows: &mut [TaskRow]) {
    let max = rows.iter().filter(|t| t.in_task).map(|t| t.usd).fold(0.0, f64::max);
    for t in rows {
        t.bar = if t.in_task && max > 0.0 && t.usd > 0.0 { ((t.usd / max * 100.0).round() as u32).max(2) } else { 0 };
    }
}

#[derive(Template)]
#[template(path = "usage_tasks.html")]
struct TasksPage {
    nav: Nav,
    days: i64,
    /// The tasks, last worked on first.
    tasks: Vec<TaskRow>,
    /// "Not in a task", one row per project, under them.
    outside: Vec<TaskRow>,
    show_status: bool,
    show_estimate: bool,
}

/// All tasks by cost: the Usage page opens it as a dialog (data-dlg); a page of its own without
/// scripts or when linked to.
async fn all_tasks(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<UsageQuery>,
    tz: Tz,
) -> AppResult<Response> {
    actor.require_owner()?;
    let days = task_days(q.days);
    let (mut tasks, outside): (Vec<TaskRow>, Vec<TaskRow>) =
        task_rows(&state, &tz, now_ms(), days).await?.into_iter().partition(|t| t.in_task);
    // Last worked on first, like every table (the owner, 2026-11-23: "by default tables are
    // sorted by "last entry" or "last chanbged""); a click on Cost sorts by cost.
    tasks.sort_by_key(|t| std::cmp::Reverse(t.last_at));
    render(TasksPage {
        nav: nav(&state, &actor, "/usage").await,
        days,
        show_status: tasks.iter().any(|t| !t.status.is_empty()),
        show_estimate: tasks.iter().any(|t| !t.estimate.is_empty()),
        tasks,
        outside,
    })
}

/// What the page needs of the plan's samples (M13).
struct PlanData {
    latest: Option<crate::plan::Sample>,
    /// Tokens since the current 5-hour window and week began.
    five_now: Tokens,
    week_now: Tokens,
    five_past: Vec<(crate::plan::Past, Tokens)>,
    week_past: Vec<(crate::plan::Past, Tokens)>,
    /// The samples in the chart's range, for the strip.
    samples: Vec<crate::plan::Sample>,
}

fn plan_data(c: &rusqlite::Connection, now: i64, from: i64, to: i64) -> anyhow::Result<PlanData> {
    use crate::plan::{FIVE_HOURS, WEEK};
    let total = |a: i64, b: i64| -> anyhow::Result<Tokens> {
        Ok(usage::per_project(c, a, b)?.iter().fold(Tokens::default(), |mut acc, (_, _, t)| {
            acc.add(t);
            acc
        }))
    };
    let latest = crate::plan::latest(c)?;
    let since = |reset: Option<i64>, len: i64| -> anyhow::Result<Tokens> {
        match reset.filter(|r| *r > now) {
            Some(r) => total(r - len, now),
            None => Ok(Tokens::default()),
        }
    };
    let five_now = since(latest.and_then(|s| s.five_reset), FIVE_HOURS)?;
    let week_now = since(latest.and_then(|s| s.week_reset), WEEK)?;
    let mut five_past = Vec::new();
    for p in crate::plan::past(c, true, now, 12)? {
        five_past.push((p, total(p.start, p.end)?));
    }
    let mut week_past = Vec::new();
    for p in crate::plan::past(c, false, now, 6)? {
        week_past.push((p, total(p.start, p.end)?));
    }
    Ok(PlanData { latest, five_now, week_now, five_past, week_past, samples: crate::plan::between(c, from, to)? })
}

/// "3 h 20 min", "4 days": how long until `t`.
fn until(t: i64, now: i64) -> String {
    let m = ((t - now).max(0) + 59_999) / 60_000;
    match m {
        0..=59 => format!("{m} min"),
        60..=1439 if m % 60 == 0 => format!("{} h", m / 60),
        60..=1439 => format!("{} h {} min", m / 60, m % 60),
        _ => {
            let d = (m + 720) / 1440;
            format!("{d} day{}", if d == 1 { "" } else { "s" })
        }
    }
}

/// The plan card's two meters and "How big a window is", from the samples.
fn plan_view(p: &PlanData, now: i64, tz: &Tz) -> (Vec<Meter>, Vec<EstRow>, (String, String)) {
    let mut meters = Vec::new();
    if let Some(s) = p.latest {
        for (label, pct, reset, so_far, past, five) in [
            ("5 hours", s.five_pct, s.five_reset, &p.five_now, &p.five_past, true),
            ("Week", s.week_pct, s.week_reset, &p.week_now, &p.week_past, false),
        ] {
            let (Some(pct), Some(reset)) = (pct, reset) else { continue };
            let over = reset <= now;
            let pct = if over { 0.0 } else { pct };
            let when = if five { tz.hm(reset) } else { tz.short(reset) };
            let est = crate::usage_report::estimate(past, so_far, pct, five);
            meters.push(Meter {
                label,
                pct: format!("{pct:.0}%"),
                width: pct.clamp(0.0, 100.0),
                warn: pct >= 80.0,
                reset: if over { format!("reset at {when}") } else { format!("{when} · in {}", until(reset, now)) },
                so_far: if over { "–".into() } else { cost_of(so_far) },
                so_far_sub: if over {
                    "no session has reported since".into()
                } else {
                    format!("{} new tokens · {} cache reads", short(so_far.new_tokens()), short(so_far.cache_read))
                },
                estimate: match &est {
                    Some((u, t, _)) => {
                        format!("100% ≈ {} at API prices ≈ {} new tokens", crate::prices::dollars(*u), short(*t as i64))
                    }
                    None => "100%: known once the window is 5% used".into(),
                },
                basis: est.map(|e| e.2).unwrap_or_default(),
            });
        }
    }
    let rows = p
        .five_past
        .iter()
        .filter(|(w, _)| w.peak >= 1.0)
        .take(6)
        .map(|(w, t)| EstRow {
            v: (w.start, w.peak, t.usd(), t.usd() / w.peak.max(1.0), t.new_tokens() as f64 / w.peak.max(1.0)),
            day: tz.date(w.start),
            span: format!("{}–{}", tz.hm(w.start), tz.hm(w.end)),
            used: format!("{:.0}%", w.peak),
            limit: w.limit_at.map(|a| format!("limit {}", tz.hm(a))).unwrap_or_default(),
            cost: cost_of(t),
            per_pct: crate::prices::dollars(t.usd() / w.peak.max(1.0)),
            tokens_per_pct: short((t.new_tokens() as f64 / w.peak.max(1.0)) as i64),
        })
        .collect();
    let median = match crate::usage_report::estimate(&p.five_past, &Tokens::default(), 0.0, true) {
        Some((u, t, _)) => (crate::prices::dollars(u / 100.0), short((t / 100.0) as i64)),
        None => (String::new(), String::new()),
    };
    (meters, rows, median)
}

/// The strip's values: the last sample in each bar's span (the week's % for days, the 5 hours'
/// for an opened day's hours).
fn strip_values(samples: &[crate::plan::Sample], bounds: &[i64], hours: bool) -> Vec<Option<f64>> {
    (0..bounds.len() - 1)
        .map(|i| {
            samples
                .iter()
                .filter(|s| s.at >= bounds[i] && s.at < bounds[i + 1])
                .filter_map(|s| if hours { s.five_pct } else { s.week_pct })
                .next_back()
        })
        .collect()
}

#[derive(serde::Deserialize)]
struct ReadForm {
    /// Where to go back to; only local paths.
    next: Option<String>,
}

/// The Reload button (the owner, 2026-11-12: "a reload button to manually refresh tokens and
/// conversations"): reads the transcripts now instead of at the next five-minute round.
async fn read_now(
    Extension(actor): Extension<crate::auth::Actor>,
    axum::Form(form): axum::Form<ReadForm>,
) -> super::AppResult<axum::response::Response> {
    actor.require_owner()?;
    super::READ_NOW.notify_one();
    let next = super::local_path(form.next.as_deref()).unwrap_or_else(|| "/usage".into());
    Ok(axum::response::IntoResponse::into_response(axum::response::Redirect::to(&next)))
}
