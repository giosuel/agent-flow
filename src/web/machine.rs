//! The Machine page: this server's load, memory and disks now, CPU and memory by project over
//! time (each agent with everything it started), and the ports projects claim. The owner,
//! 2026-11-07: "agent flow will only ever run on one machine, so it can show diagnostics of the
//! current machine like CPU / MEM history and how the agents use it".

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
use crate::machine;
use crate::util::now_ms;

pub fn routes() -> Router<AppState> {
    Router::new().route("/machine", get(page))
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
}

fn gb(bytes: u64) -> f64 {
    bytes as f64 / 1e9
}

async fn page(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Query(q): Query<MachineQuery>,
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
    let today = crate::usage::day_start(now);
    let (projects, history, summary) = state
        .db
        .call(move |c| {
            Ok((crate::projects::list(c)?, machine::history(c, from, step)?, machine::group_summary(c, today)?))
        })
        .await?;

    let snap = machine::snapshot();
    let (l1, l5, l15) = machine::loads();
    let (root_free, root_total) = machine::disk("/");
    let (tmp_free, tmp_total) = machine::disk("/tmp");
    let tiles = vec![
        Tile {
            label: "CPU now",
            value: format!("{l1:.1} of {} cores", snap.cores),
            warn: l1 > snap.cores as f64 * 0.9,
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
    ];

    let slugs: Vec<String> = projects.iter().map(|p| p.slug.clone()).collect();
    let colors = charts::colors(&slugs);
    let mut groups: Vec<(String, String, &'static str)> =
        projects.iter().zip(&colors).map(|(p, c)| (p.slug.clone(), p.name.clone(), *c)).collect();
    groups.push(("other".into(), "Other (system, services, other users)".into(), charts::OTHER));
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
    let (cpu, cpu_max) = charts::stacked_areas("CPU by project, cores", n, &cpu_series, Some(snap.cores as f64));
    let (mem, mem_max) = charts::stacked_areas("Memory by project, GB", n, &mem_series, Some(snap.mem_total_gb()));
    let legend = cpu_series.iter().map(|s| Legend { name: s.name.clone(), color: s.color }).collect();
    let x_labels: Vec<String> = (0..5)
        .map(|i| {
            let t = from + span * i / 4;
            let s = crate::util::fmt_utc(t);
            if range == "7d" { s[5..10].to_owned() } else { s[11..16].to_owned() }
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
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim().to_owned();
    render(MachinePage {
        nav: nav(&state, &actor, "/machine").await,
        host: format!("{host} · {} cores · {:.0} GB RAM", snap.cores, snap.mem_total_gb()),
        tiles,
        ranges: vec![("6h", "6 h", range == "6h"), ("24h", "24 h", range == "24h"), ("7d", "7 d", range == "7d")],
        sampled: !history.is_empty(),
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
