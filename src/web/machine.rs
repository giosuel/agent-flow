//! The Machine page: this server's load, memory and disks now, CPU and memory by project over
//! time (each agent with everything it started), and the ports projects claim. The owner,
//! 2026-11-07: "agent flow will only ever run on one machine, so it can show diagnostics of the
//! current machine like CPU / MEM history and how the agents use it".

use askama::Template;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Router};
use serde::Deserialize;

use super::charts::{self, Series};
use super::pages::{Nav, nav, render};
use super::{AppResult, AppState};
use crate::auth::Actor;
use crate::machine;
use crate::util::now_ms;

pub fn routes() -> Router<AppState> {
    Router::new().route("/machine", get(page)).route("/machine/now", get(now))
}

#[derive(Deserialize)]
struct MachineQuery {
    range: Option<String>,
}

struct Tile {
    label: &'static str,
    value: String,
    warn: bool,
}

struct Legend {
    name: String,
    color: &'static str,
}

struct AgentRow {
    name: String,
    color: &'static str,
    cpu_hours: String,
    peak: String,
    now: String,
}

struct PortRow {
    port: String,
    project: String,
}

#[derive(Template)]
#[template(path = "machine.html")]
struct MachinePage {
    nav: Nav,
    host: String,
    tiles: Vec<Tile>,
    ranges: Vec<(&'static str, &'static str, bool)>,
    legend: Vec<Legend>,
    cpu: String,
    cpu_max: String,
    mem: String,
    mem_max: String,
    x_labels: Vec<String>,
    agents: Vec<AgentRow>,
    ports: Vec<PortRow>,
    sampled: bool,
    /// "Collecting since 05:20 (12 min so far)" while the samples don't cover the range yet.
    collecting: Option<String>,
    cores: usize,
    mem_total: String,
    /// The Build servers section, when there are any.
    builds: String,
}

/// The figures at the top, now. The page asks for them again every 5 seconds (`/machine/now`).
async fn tiles(snap: &machine::Snapshot) -> Vec<Tile> {
    let busy = machine::busy_cores(snap.cores).await;
    let (l1, l5, l15) = machine::loads();
    let (root_free, root_total) = machine::disk("/");
    let (tmp_free, tmp_total) = machine::disk("/tmp");
    vec![
        Tile {
            label: "CPU now",
            value: format!("{busy:.1} of {} cores", snap.cores),
            warn: busy > snap.cores as f64 * 0.9,
        },
        Tile {
            label: "Memory now",
            value: format!("{:.1} of {:.0} GB", snap.mem_used_gb(), snap.mem_total_gb()),
            warn: snap.mem_available_kb < 2 * 1024 * 1024,
        },
        Tile {
            label: "Disk free",
            value: format!("{:.0} of {:.0} GB", gb(root_free), gb(root_total)),
            warn: root_free < 15_000_000_000,
        },
        Tile {
            label: "/tmp free",
            value: format!("{:.1} of {:.0} GB", gb(tmp_free), gb(tmp_total)),
            warn: tmp_free < 2_000_000_000,
        },
        Tile { label: "Load 1 / 5 / 15", value: format!("{l1:.1}  {l5:.1}  {l15:.1}"), warn: false },
    ]
}

/// The tiles as JSON, for the page's live update.
async fn now(Extension(actor): Extension<Actor>) -> AppResult<Response> {
    actor.require_owner()?;
    let tiles = tiles(&machine::snapshot()).await;
    let body = serde_json::json!({ "tiles": tiles.iter().map(|t| serde_json::json!({"value": t.value, "warn": t.warn})).collect::<Vec<_>>() });
    Ok(axum::Json(body).into_response())
}

fn gb(bytes: u64) -> f64 {
    bytes as f64 / 1e9
}

async fn page(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<MachineQuery>,
    tz: crate::tz::Tz,
) -> AppResult<Response> {
    actor.require_owner()?;
    let now = now_ms();
    let (range, span, step) = match q.range.as_deref() {
        Some("6h") => ("6h", 6 * 3_600_000, 5 * 60_000),
        Some("7d") => ("7d", 7 * 86_400_000, 2 * 3_600_000),
        _ => ("24h", 86_400_000, 15 * 60_000),
    };
    let n = (span / step) as usize;
    let from = now - span;
    let today = tz.day_start(now);
    let (projects, history, summary, first) = state
        .db
        .call(move |c| {
            let first: Option<i64> = c.query_row("SELECT min(at) FROM machine_samples", [], |r| r.get(0))?;
            Ok((crate::projects::list(c)?, machine::history(c, from, step)?, machine::group_summary(c, today)?, first))
        })
        .await?;

    let snap = machine::snapshot();
    let tiles = tiles(&snap).await;

    let slugs: Vec<String> = projects.iter().map(|p| p.slug.clone()).collect();
    let colors = charts::colors(&slugs);
    let mut groups: Vec<(String, String, &'static str)> =
        projects.iter().zip(&colors).map(|(p, c)| (p.slug.clone(), p.name.clone(), *c)).collect();
    groups.push(("other".into(), "Other".into(), charts::OTHER));
    let series_of = |pick: &dyn Fn(f64, f64) -> f64| -> Vec<Series> {
        groups
            .iter()
            .filter_map(|(g, name, color)| {
                let mut values = vec![0.0; n];
                for (b, grp, cores, mem) in &history {
                    if grp == g
                        && let Some(v) = values.get_mut(*b as usize)
                    {
                        *v = pick(*cores, *mem);
                    }
                }
                values.iter().any(|v| *v > 0.0).then(|| Series { name: name.clone(), color, values })
            })
            .collect()
    };
    let cpu_series = series_of(&|cores, _| cores);
    let mem_series = series_of(&|_, mem_kb| mem_kb / 1024.0 / 1024.0);
    // Each point's time, for the hover layer: "14:05", or "Tue 10 14:00" over a week.
    let point_labels: Vec<String> = (0..n as i64)
        .map(|i| {
            let t = from + i * step;
            if range == "7d" { format!("{} {}", tz.day_label(t), tz.hm(t)) } else { tz.hm(t) }
        })
        .collect();
    let cores_fmt = |v: f64| format!("{v:.1} cores");
    let gb_fmt = |v: f64| format!("{v:.1} GB");
    let (cpu, cpu_max) =
        charts::stacked_areas("CPU by project, cores", &point_labels, &cpu_series, Some(snap.cores as f64), &cores_fmt);
    let (mem, mem_max) =
        charts::stacked_areas("Memory by project, GB", &point_labels, &mem_series, Some(snap.mem_total_gb()), &gb_fmt);
    let legend = cpu_series.iter().map(|s| Legend { name: s.name.clone(), color: s.color }).collect();
    let x_labels: Vec<String> = (0..5)
        .map(|i| {
            let t = from + span * i / 4;
            if range == "7d" { tz.date(t).get(5..).unwrap_or("").to_owned() } else { tz.hm(t) }
        })
        .collect();

    let agents = groups
        .iter()
        .filter_map(|(g, name, color)| {
            let (_, cpu_h, peak, now_cores, top) = summary.iter().find(|s| &s.0 == g)?;
            Some(AgentRow {
                name: name.clone(),
                color,
                cpu_hours: format!("{cpu_h:.1}"),
                peak: format!("{:.1} GB", *peak as f64 / 1024.0 / 1024.0),
                now: if *now_cores < 0.05 { "idle".into() } else { format!("{now_cores:.1} cores · {top}") },
            })
        })
        .collect();
    let ports = projects
        .iter()
        .flat_map(|p| p.ports.iter().map(move |port| PortRow { port: port.clone(), project: p.name.clone() }))
        .collect();
    let host = machine::hostname();
    let builds = if state.config.build_servers.is_empty() {
        String::new()
    } else {
        use askama::Template as _;
        super::remote::section_data(&state, &actor).await?.render().map_err(anyhow::Error::from)?
    };
    render(MachinePage {
        builds,
        nav: nav(&state, &actor, "/machine").await,
        host: format!("{host} · {} cores · {:.0} GB RAM", snap.cores, snap.mem_total_gb()),
        tiles,
        ranges: vec![("6h", "6 h", range == "6h"), ("24h", "24 h", range == "24h"), ("7d", "7 d", range == "7d")],
        sampled: !history.is_empty(),
        collecting: first.filter(|f| *f > from + step).map(|f| {
            format!(
                "Collecting since {} ({} so far); the chart fills in as it goes.",
                tz.hm(f),
                crate::util::ago(f, now).trim_end_matches(" ago")
            )
        }),
        cores: snap.cores,
        mem_total: format!("{:.0}", snap.mem_total_gb()),
        legend,
        cpu,
        cpu_max: format!("{cpu_max:.0}"),
        mem,
        mem_max: format!("{mem_max:.0}"),
        x_labels,
        agents,
        ports,
    })
}
