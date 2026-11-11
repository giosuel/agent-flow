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
    /// Its icon (see icons.rs): cpu, memory, disk, tmp, load.
    icon: &'static str,
    label: &'static str,
    /// The number, large, and what follows it: "2.0" "of 8 cores".
    big: String,
    small: String,
    /// How it moved in the last hour: "↗ from 1.2 an hour ago", "→ steady".
    trend: String,
    /// The last hour as a filled line (SVG), 0..1 of the card's scale.
    spark: String,
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

/// The figures at the top, now, each with its last hour. The page asks for the numbers again every
/// 5 seconds (`/machine/now`) and for the cards every minute.
fn tiles(snap: &machine::Snapshot, now: &machine::Totals, hour: &[machine::Totals]) -> Vec<Tile> {
    let cores = snap.cores as f64;
    let mem_total = snap.mem_total_gb();
    let (_, root_total) = machine::disk("/");
    let (_, tmp_total) = machine::disk("/tmp");
    let (root_total, tmp_total) = (gb(root_total), gb(tmp_total));
    let (_, l5, l15) = machine::loads();
    // The oldest sample of the hour, if the hour is mostly there.
    let then = hour.first().filter(|t| now.at - t.at > 45 * 60_000);
    let trend = |now_v: f64, then_v: Option<f64>, steady: f64, fmt: &dyn Fn(f64) -> String| match then_v {
        None => "the last hour fills in as it goes".to_owned(),
        Some(t) if (now_v - t).abs() < steady => "→ steady".to_owned(),
        Some(t) if now_v > t => format!("↗ from {} an hour ago", fmt(t)),
        Some(t) => format!("↘ from {} an hour ago", fmt(t)),
    };
    let line = |f: &dyn Fn(&machine::Totals) -> f64, warn: bool| spark(&hour.iter().map(f).collect::<Vec<_>>(), warn);
    let disk_warn = now.disk_free_gb < 15.0 || now.disk_free_gb < root_total * 0.1;
    let tmp_warn = now.tmp_free_gb < 2.0;
    let cpu_warn = now.cpu > cores * 0.9;
    let mem_warn = snap.mem_available_kb < 2 * 1024 * 1024;
    vec![
        Tile {
            icon: "cpu",
            label: "CPU",
            big: format!("{:.1}", now.cpu),
            small: format!("of {} cores", snap.cores),
            trend: trend(now.cpu, then.map(|t| t.cpu), cores * 0.1, &|v| format!("{v:.1}")),
            spark: line(&|t| t.cpu / cores.max(1.0), cpu_warn),
            warn: cpu_warn,
        },
        Tile {
            icon: "memory",
            label: "Memory",
            big: format!("{:.1}", now.mem_gb),
            small: format!("of {mem_total:.0} GB"),
            trend: trend(now.mem_gb, then.map(|t| t.mem_gb), mem_total * 0.05, &|v| format!("{v:.1} GB")),
            spark: line(&|t| t.mem_gb / mem_total.max(1.0), mem_warn),
            warn: mem_warn,
        },
        Tile {
            icon: "disk",
            label: "Disk",
            big: format!("{:.0}", now.disk_free_gb),
            small: format!("GB free of {root_total:.0}"),
            trend: trend(now.disk_free_gb, then.map(|t| t.disk_free_gb), 1.0, &|v| format!("{v:.0} GB free")),
            spark: line(&|t| 1.0 - t.disk_free_gb / root_total.max(1.0), disk_warn),
            warn: disk_warn,
        },
        Tile {
            icon: "tmp",
            label: "/tmp",
            big: format!("{:.1}", now.tmp_free_gb),
            small: format!("GB free of {tmp_total:.0}"),
            trend: trend(now.tmp_free_gb, then.map(|t| t.tmp_free_gb), 0.5, &|v| format!("{v:.1} GB free")),
            spark: line(&|t| 1.0 - t.tmp_free_gb / tmp_total.max(1.0), tmp_warn),
            warn: tmp_warn,
        },
        Tile {
            icon: "load",
            label: "Load",
            big: format!("{:.1}", now.load1),
            small: format!("· {l5:.1} · {l15:.1}"),
            trend: trend(now.load1, then.map(|t| t.load1), cores * 0.1, &|v| format!("{v:.1}")),
            spark: line(&|t| t.load1 / cores.max(1.0), false),
            warn: false,
        },
    ]
}

/// A filled line of `vals` (each 0..1), 60 points wide.
fn spark(vals: &[f64], warn: bool) -> String {
    if vals.len() < 2 {
        return String::new();
    }
    let (w, h) = (160.0, 36.0);
    let n = (vals.len() - 1) as f64;
    let pts: Vec<String> = vals
        .iter()
        .enumerate()
        .map(|(i, v)| format!("{:.1},{:.1}", i as f64 * w / n, h - v.clamp(0.0, 1.0) * (h - 2.0) - 1.0))
        .collect();
    let color = if warn { "var(--warn)" } else { "var(--accent)" };
    format!(
        r#"<svg class="spark" viewBox="0 0 160 36" preserveAspectRatio="none" aria-hidden="true"><polygon points="0,36 {p} 160,36" fill="{color}" opacity=".16"/><polyline points="{p}" fill="none" stroke="{color}" stroke-width="2" vector-effect="non-scaling-stroke"/></svg>"#,
        p = pts.join(" ")
    )
}

async fn hour_and_now(state: &AppState) -> (machine::Totals, Vec<machine::Totals>) {
    let now = machine::totals_now().await;
    let from = now.at - 3_600_000;
    let mut hour = state.db.call(move |c| machine::totals_since(c, from)).await.unwrap_or_default();
    hour.push(now);
    (now, hour)
}

/// The tiles as JSON, for the page's live update.
async fn now(State(state): State<AppState>, Extension(actor): Extension<Actor>) -> AppResult<Response> {
    actor.require_owner()?;
    let (now, hour) = hour_and_now(&state).await;
    let tiles = tiles(&machine::snapshot(), &now, &hour);
    let body = serde_json::json!({ "tiles": tiles.iter().map(|t| serde_json::json!({"big": t.big, "small": t.small, "warn": t.warn})).collect::<Vec<_>>() });
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
    let (now_t, hour) = hour_and_now(&state).await;
    let tiles = tiles(&snap, &now_t, &hour);

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
    let builds = if crate::remote::servers(&state).is_empty() {
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
