//! The Usage page: token counts per day (an hour per bar when a day is opened), one line per
//! agent, a table per project, and a Tasks tab with each task's tokens.

use askama::Template;
use axum::extract::{Query, State};
use axum::response::Response;
use axum::routing::get;
use axum::{Extension, Router};
use serde::Deserialize;

use super::charts::{self, Series};
use super::pages::{Nav, nav, render};
use super::{AppResult, AppState};
use crate::auth::Actor;
use crate::records::{self, Kind};
use crate::usage::{self, DAY, HOUR, Tokens, short};
use crate::util::now_ms;

pub fn routes() -> Router<AppState> {
    Router::new().route("/usage", get(page))
}

#[derive(Deserialize, Default)]
struct UsageQuery {
    tab: Option<String>,
    /// "2026-11-10": that day's hours instead of 14 days.
    day: Option<String>,
    /// "cache" for cache reads; new tokens otherwise.
    metric: Option<String>,
    /// Tasks tab: 7 or 14 days.
    days: Option<i64>,
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
}

impl Cells {
    fn of(t: &Tokens) -> Self {
        Self {
            calls: t.calls.to_string(),
            input: short(t.input),
            cache_write: short(t.cache_write),
            output: short(t.output),
            new_tokens: short(t.new_tokens()),
            cache_read: short(t.cache_read),
        }
    }
}

struct TaskRow {
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
    label: &'static str,
    value: String,
    hint: String,
}

#[derive(Template)]
#[template(path = "usage.html")]
struct UsagePage {
    nav: Nav,
    tab: String,
    newest: String,
    tiles: Vec<Tile>,
    chart_title: String,
    chart_sub: String,
    bars: String,
    bars_max: String,
    x_labels: Vec<String>,
    zoomed: bool,
    cache: bool,
    /// Query string that keeps the day while switching the metric.
    day_q: String,
    legend: Vec<Legend>,
    lines: String,
    lines_max: String,
    projects: Vec<ProjectRow>,
    range: String,
    tasks: Vec<TaskRow>,
    days: i64,
    empty: bool,
}

fn date_of(ms: i64) -> String {
    crate::util::fmt_utc(ms).get(..10).unwrap_or("").to_owned()
}

/// "Mon 10" for a day's bar label.
fn day_label(ms: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp(ms / 1000).unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let wd = t.weekday().to_string();
    format!("{} {}", &wd[..3], t.day())
}

fn parse_day(s: &str) -> Option<i64> {
    let f = time::macros::format_description!("[year]-[month]-[day]");
    let d = time::Date::parse(s, &f).ok()?;
    Some(d.midnight().assume_utc().unix_timestamp() * 1000)
}

async fn page(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<UsageQuery>,
) -> AppResult<Response> {
    actor.require_owner()?;
    let now = now_ms();
    let today = usage::day_start(now);
    let tab = if q.tab.as_deref() == Some("tasks") { "tasks" } else { "overview" }.to_owned();
    let cache = q.metric.as_deref() == Some("cache");
    let day = q.day.as_deref().and_then(parse_day).filter(|d| *d <= today);
    let days = if q.days == Some(7) { 7 } else { 14 };
    let (from, to, step, n) = match day {
        Some(d) => (d, d + DAY, HOUR, 24usize),
        None => (today - 13 * DAY, today + DAY, DAY, 14usize),
    };
    let task_from = today - (days - 1) * DAY;
    let data = state
        .db
        .call(move |c| {
            let projects = crate::projects::list(c)?;
            let buckets = usage::buckets(c, from, to, step)?;
            let per_project = usage::per_project(c, from, to)?;
            let tasks = usage::per_task(c, task_from, today + DAY)?;
            let mut task_records = Vec::new();
            for t in &tasks {
                task_records.push(match t.task_id {
                    Some(id) => Some(records::get(c, id)?),
                    None => None,
                });
            }
            let today_total = usage::per_project(c, today, today + DAY)?;
            let fortnight = usage::per_project(c, today - 13 * DAY, today + DAY)?;
            let newest: Option<i64> = c.query_row("SELECT max(at) FROM usage", [], |r| r.get(0))?;
            Ok((projects, buckets, per_project, tasks, task_records, today_total, fortnight, newest))
        })
        .await?;
    let (projects, buckets, per_project, tasks, task_records, today_total, fortnight, newest) = data;

    let slugs: Vec<String> = projects.iter().map(|p| p.slug.clone()).collect();
    let colors = charts::colors(&slugs);
    let name_of = |id: Option<i64>| -> (String, &'static str) {
        match id.and_then(|id| projects.iter().position(|p| p.id == id)) {
            Some(i) => (projects[i].name.clone(), colors[i]),
            None => ("Other".into(), charts::OTHER),
        }
    };
    let metric = |t: &Tokens| if cache { t.cache_read } else { t.new_tokens() } as f64;

    // One series per project that has any tokens in the window, in project order; Other last.
    let mut ids: Vec<Option<i64>> = projects.iter().map(|p| Some(p.id)).collect();
    ids.push(None);
    let mut series = Vec::new();
    let mut legend = Vec::new();
    for id in ids {
        let mut values = vec![0.0; n];
        for (b, p, t) in &buckets {
            if *p == id
                && let Some(v) = values.get_mut(*b as usize)
            {
                *v += metric(t);
            }
        }
        let total: f64 = values.iter().sum();
        if total <= 0.0 {
            continue;
        }
        let (name, color) = name_of(id);
        legend.push(Legend { name: name.clone(), color, total: short(total as i64) });
        series.push(Series { name, color, values });
    }
    let labels: Vec<String> =
        (0..n).map(|i| if day.is_some() { format!("{i:02}:00") } else { day_label(from + i as i64 * DAY) }).collect();
    let metric_q = if cache { "&metric=cache" } else { "" };
    let link = move |i: usize| format!("/usage?day={}{metric_q}", date_of(from + i as i64 * DAY));
    let fmt = |v: f64| short(v as i64);
    let what = if cache { "cache reads" } else { "new tokens" };
    let (bars, bars_max) = charts::stacked_bars(
        &format!("{what} per {}", if day.is_some() { "hour" } else { "day" }),
        &labels,
        &series,
        &fmt,
        if day.is_none() { Some(&link) } else { None },
    );
    // The per-agent lines: the same buckets, a cumulative line per project would hide the
    // shape, so these are per bucket too.
    let (lines, lines_max) = charts::lines("per agent", n, &series);
    let x_labels: Vec<String> = labels
        .iter()
        .enumerate()
        .map(|(i, l)| if day.is_some() && i % 3 != 0 { String::new() } else { l.clone() })
        .collect();

    let sum = |rows: &[(Option<i64>, i64, Tokens)]| {
        rows.iter().fold(Tokens::default(), |mut a, (_, _, t)| {
            a.calls += t.calls;
            a.input += t.input;
            a.cache_write += t.cache_write;
            a.output += t.output;
            a.cache_read += t.cache_read;
            a
        })
    };
    let (td, fn_) = (sum(&today_total), sum(&fortnight));
    let tiles = vec![
        Tile { label: "New tokens today", value: short(td.new_tokens()), hint: format!("{} calls", td.calls) },
        Tile { label: "New tokens, 14 days", value: short(fn_.new_tokens()), hint: format!("{} calls", fn_.calls) },
        Tile {
            label: "Cache reads, 14 days",
            value: short(fn_.cache_read),
            hint: if fn_.new_tokens() > 0 {
                format!("{:.0}× the new tokens", fn_.cache_read as f64 / fn_.new_tokens() as f64)
            } else {
                String::new()
            },
        },
    ];

    let mut project_rows: Vec<ProjectRow> = per_project
        .iter()
        .map(|(id, sessions, t)| {
            let (name, color) = name_of(*id);
            ProjectRow { name, color, sessions: *sessions, t: Cells::of(t) }
        })
        .collect();
    project_rows.sort_by(|a, b| (a.name == "Other").cmp(&(b.name == "Other")).then(a.name.cmp(&b.name)));

    let task_rows: Vec<TaskRow> = tasks
        .iter()
        .zip(&task_records)
        .map(|(t, r)| {
            let (project, color) = name_of(t.project_id);
            let worked = if usage::day_start(t.first_at) == usage::day_start(t.last_at) {
                format!(
                    "{} {}–{}",
                    date_of(t.first_at),
                    &crate::util::fmt_utc(t.first_at)[11..16],
                    &crate::util::fmt_utc(t.last_at)[11..16]
                )
            } else {
                format!("{} – {}", date_of(t.first_at), date_of(t.last_at))
            };
            let total = t.total();
            TaskRow {
                id: r.as_ref().map(|r| r.short_id()).unwrap_or_default(),
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

    let day_q = day.map(|d| format!("day={}&", date_of(d))).unwrap_or_default();
    render(UsagePage {
        nav: nav(&state, &actor, "/usage").await,
        tab,
        newest: newest.map(|t| crate::util::ago(t, now)).unwrap_or_else(|| "not yet".into()),
        tiles,
        chart_title: match day {
            Some(d) => format!("{} by hour · {}", if cache { "Cache reads" } else { "New tokens" }, date_of(d)),
            None => format!("{} per day · 14 days", if cache { "Cache reads" } else { "New tokens" }),
        },
        chart_sub: if day.is_some() { "UTC hours".into() } else { "Open a day for its hours".into() },
        bars,
        bars_max: short(bars_max as i64),
        x_labels,
        zoomed: day.is_some(),
        cache,
        day_q,
        empty: legend.is_empty(),
        legend,
        lines,
        lines_max: short(lines_max as i64),
        projects: project_rows,
        range: match day {
            Some(d) => date_of(d),
            None => "14 days".into(),
        },
        tasks: task_rows,
        days,
    })
}
