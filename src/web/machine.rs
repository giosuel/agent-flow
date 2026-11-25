//! The Machines page: this server's load, memory and disks now, and the build servers. The owner,
//! 2026-11-07: "agent flow will only ever run on one machine, so it can show diagnostics of the
//! current machine like CPU / MEM history and how the agents use it". The CPU and memory by
//! project and the claimed ports went (D-283: "I think we can remove the "by project" for now");
//! the samples per project are still taken (the Dashboard's tile reads them).

use askama::Template;
use axum::extract::State;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Extension, Router};

use super::pages::{Nav, nav, render};
use super::{AppResult, AppState};
use crate::auth::Actor;
use crate::machine;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/machines", get(page))
        .route("/machine/now", get(now))
        // The Machine page became Machines (D-277): its old address leads there.
        .route("/machine", get(|| async { Redirect::to("/machines") }))
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

#[derive(Template)]
#[template(path = "machine.html")]
struct MachinePage {
    nav: Nav,
    host: String,
    tiles: Vec<Tile>,
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

async fn page(State(state): State<AppState>, Extension(actor): Extension<Actor>) -> AppResult<Response> {
    actor.require_owner()?;
    let snap = machine::snapshot();
    let (now_t, hour) = hour_and_now(&state).await;
    let tiles = tiles(&snap, &now_t, &hour);
    let host = machine::hostname();
    let builds = if crate::remote::servers(&state).is_empty() {
        String::new()
    } else {
        use askama::Template as _;
        super::remote::section_data(&state, &actor).await?.render().map_err(anyhow::Error::from)?
    };
    render(MachinePage {
        builds,
        nav: nav(&state, &actor, "/machines").await,
        host: format!("{host} · {} cores · {:.0} GB RAM", snap.cores, snap.mem_total_gb()),
        tiles,
    })
}
