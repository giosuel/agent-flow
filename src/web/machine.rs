//! The Machines pages (D-277): an overview of AgentFlow's own server (its load, memory and disks
//! now) and of every build machine, and a page per build machine with its jobs. The owner,
//! 2026-11-25: "each machine should also get its own page with the build history. remove the
//! overall build history and make the "machines" page purely an overview of all build machines
//! and their stats as well as the agentflow server stats". The CPU and memory by project and the
//! claimed ports went (D-283); the samples per project are still taken (the Dashboard's tile reads
//! them). The pages swap their live parts in every 5 seconds (static/machine.js).

use askama::Template;
use axum::extract::State;
use axum::response::{Redirect, Response};
use axum::routing::get;
use axum::{Extension, Router};

use super::pages::{Nav, nav, render};
use super::remote::{Meter, num, took};
use super::{AppResult, AppState};
use crate::auth::Actor;
use crate::config::BuildServer;
use crate::machine;
use crate::remote::{self, Job};
use crate::util::{ago, now_ms};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/machines", get(overview))
        // The Machine page became Machines (D-277): its old addresses lead there (a page open since
        // then asks for its parts at the old ones, and finds them in the new page).
        .route("/machine", get(|| async { Redirect::to("/machines") }))
        .route("/machine/now", get(|| async { Redirect::to("/machines") }))
        .route("/machine/builds", get(|| async { Redirect::to("/machines") }))
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

/// AgentFlow's server now, each figure with its last hour.
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

fn gb(bytes: u64) -> f64 {
    bytes as f64 / 1e9
}

// ---------------------------------------------------------------------------------------------
// Build machines

/// A job running or waiting on a build machine.
pub struct NowJob {
    pub id: i64,
    /// Its name, or its command for a job from before names (then shown as code).
    pub title: String,
    pub named: bool,
    pub project: String,
    /// "imaginarium · 22 threads"
    pub sub: String,
    /// Its chip: tone and words ("running 4:12", "waiting for 16 free threads").
    pub chip: &'static str,
    pub label: String,
}

impl NowJob {
    fn of(j: &Job) -> Self {
        let (chip, label) = match j.state.as_str() {
            "running" => ("accent", format!("running {}", took(j))),
            "paused" if j.reason.is_empty() => ("warn", format!("paused {}", took(j))),
            "paused" => ("warn", format!("paused · {}", j.reason)),
            _ if j.reason.is_empty() => ("", "waiting".to_owned()),
            _ => ("", j.reason.clone()),
        };
        let mut sub = j.project.clone();
        if let Some(t) = j.threads.filter(|_| j.state != "queued") {
            sub.push_str(&format!(" · {t} threads"));
        }
        NowJob { id: j.id, title: j.title(), named: !j.name.is_empty(), project: j.project.clone(), sub, chip, label }
    }
}

/// A build machine as the Machines pages show it: what AgentFlow knows of it now.
pub struct BuildMachine {
    pub name: String,
    pub online: bool,
    /// The owner paused its builds: nothing starts, running jobs are frozen.
    pub paused: bool,
    /// "32 threads · 92 GB · RTX 5090"; empty before it was ever seen.
    pub specs: String,
    /// "online", "offline · last seen 2 days ago".
    pub state: String,
    pub meters: Vec<Meter>,
    /// Running (or frozen) jobs, then the waiting ones, the oldest first.
    pub running: Vec<NowJob>,
    pub waiting: Vec<NowJob>,
    /// Nothing running: "idle; the last job finished 12 min ago", "nothing" when offline.
    pub idle: String,
    /// The builder's version while it's connected.
    pub version: String,
    pub counts: remote::Counts,
    /// Its limits and folders, from Settings.
    pub server: BuildServer,
    /// Connected since, or last seen (ms).
    pub since: Option<i64>,
    pub seen: i64,
    pub threads: f64,
    pub memory_gb: f64,
    pub gpu: String,
    /// Its work folders' sizes, as measured last ("3 projects, 96 GB (…)").
    pub folders: String,
}

impl BuildMachine {
    /// Beside its name: "online · 32 threads · 92 GB · RTX 5090".
    pub fn facts(&self) -> String {
        let mut f = vec![self.state.clone()];
        if self.paused {
            f.push("paused".into());
        }
        if !self.specs.is_empty() && self.online {
            f.push(self.specs.clone());
        }
        f.join(" · ")
    }

    /// Its card's foot: "Today 9 jobs, 1 failed · builder 0.0.0-dev+g4be65a8".
    pub fn foot(&self) -> String {
        let c = &self.counts;
        let mut f = match (c.today, c.today_failed) {
            (0, _) => "No jobs today".to_owned(),
            (n, 0) => format!("Today {n} job{}", if n == 1 { "" } else { "s" }),
            (n, k) => format!("Today {n} job{}, {k} failed", if n == 1 { "" } else { "s" }),
        };
        if !self.version.is_empty() {
            f.push_str(&format!(" · builder {}", self.version));
        }
        f
    }
}

/// The build machines (or the one called `only`) as they are now. Asking keeps their probes going
/// (see `remote::Live::watched_at`).
pub async fn build_machines(state: &AppState, only: Option<&str>, today: i64) -> anyhow::Result<Vec<BuildMachine>> {
    state.remote.lock().unwrap_or_else(|e| e.into_inner()).watched_at = now_ms();
    let list: Vec<BuildServer> =
        remote::servers(state).into_iter().filter(|b| only.is_none_or(|o| b.name == o)).collect();
    let names: Vec<String> = list.iter().map(|b| b.name.clone()).collect();
    let db: Vec<(Vec<Job>, remote::Counts)> = state
        .db
        .call(move |c| {
            names
                .iter()
                .map(|n| Ok((remote::in_state(c, n, &["running", "paused", "queued"])?, remote::counts(c, n, today)?)))
                .collect()
        })
        .await?;
    let now = now_ms();
    let hub = crate::builders::hub();
    let live = state.remote.lock().unwrap_or_else(|e| e.into_inner());
    Ok(list
        .into_iter()
        .zip(db)
        .map(|(b, (jobs, counts))| {
            let l = live.servers.get(&b.name);
            let p = l.map(|l| l.probe.clone()).unwrap_or_default();
            let online = hub.is_online(&b.name);
            let hello = hub.hello(&b.name);
            // The probe's figures, or what the builder said when it connected.
            let (threads, memory_gb) = match (&hello, p.threads > 0.0) {
                (_, true) => (p.threads, p.mem_total_gb),
                (Some((_, t, m)), false) => (*t as f64, *m),
                (None, false) => (0.0, 0.0),
            };
            let gpu = p.gpu_name.replace("NVIDIA GeForce ", "");
            let mut specs = Vec::new();
            if threads > 0.0 {
                specs.push(format!("{} threads", num(threads)));
                specs.push(format!("{memory_gb:.0} GB"));
            }
            if !gpu.is_empty() {
                specs.push(gpu.clone());
            }
            let mut meters = Vec::new();
            if online && p.online && p.threads > 0.0 {
                meters.push(Meter {
                    label: "CPU",
                    you: p.yours(),
                    builds: p.builds,
                    total: p.threads,
                    unit: "threads",
                });
                meters.push(Meter {
                    label: "Memory",
                    you: p.mem_yours_gb(),
                    builds: p.mem_builds_gb,
                    total: p.mem_total_gb,
                    unit: "GB",
                });
                if !p.gpu_name.is_empty() {
                    meters.push(Meter { label: "GPU", you: p.gpu_util, builds: 0.0, total: 100.0, unit: "%" });
                }
            }
            let (running, waiting): (Vec<&Job>, Vec<&Job>) = jobs.iter().partition(|j| j.state != "queued");
            let idle = match counts.last_finished {
                _ if !online => "nothing".to_owned(),
                Some(t) => format!("idle; the last job finished {}", ago(t, now)),
                None => "idle".to_owned(),
            };
            BuildMachine {
                state: if online {
                    "online".to_owned()
                } else if p.at > 0 {
                    format!("offline · last seen {}", ago(p.at, now))
                } else {
                    "offline".to_owned()
                },
                name: b.name.clone(),
                online,
                paused: l.is_some_and(|l| l.paused),
                specs: specs.join(" · "),
                meters,
                running: running.into_iter().map(NowJob::of).collect(),
                waiting: waiting.into_iter().map(NowJob::of).collect(),
                idle,
                version: hello.map(|h| h.0).unwrap_or_default(),
                counts,
                since: hub.since(&b.name),
                seen: p.at,
                threads,
                memory_gb,
                gpu,
                folders: l.map(|l| super::remote::folders(&l.sizes)).unwrap_or_default(),
                server: b,
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------------------------
// The overview

#[derive(Template)]
#[template(path = "machines.html")]
struct Overview {
    nav: Nav,
    /// "AgentFlow runs on giosuelio", "3 build machines, 2 online", "1 job running, 2 waiting".
    status: Vec<String>,
    host: String,
    /// "AgentFlow's server · 8 cores · 23 GB"
    host_sub: String,
    tiles: Vec<Tile>,
    machines: Vec<BuildMachine>,
}

/// The status line's parts.
fn status(host: &str, machines: &[BuildMachine]) -> Vec<String> {
    let mut s = vec![format!("AgentFlow runs on {host}")];
    let online = machines.iter().filter(|m| m.online).count();
    s.push(match machines.len() {
        0 => "no build machines yet".to_owned(),
        1 => format!("1 build machine, {}", if online == 1 { "online" } else { "offline" }),
        n => format!("{n} build machines, {online} online"),
    });
    if !machines.is_empty() {
        let running: usize = machines.iter().map(|m| m.running.len()).sum();
        let waiting: usize = machines.iter().map(|m| m.waiting.len()).sum();
        s.push(match (running, waiting) {
            (0, 0) => "nothing running".to_owned(),
            (0, w) => format!("nothing running, {w} waiting"),
            (r, 0) => format!("{r} job{} running", if r == 1 { "" } else { "s" }),
            (r, w) => format!("{r} job{} running, {w} waiting", if r == 1 { "" } else { "s" }),
        });
    }
    s
}

async fn overview(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    tz: crate::tz::Tz,
) -> AppResult<Response> {
    actor.require_owner()?;
    let snap = machine::snapshot();
    let (now_t, hour) = hour_and_now(&state).await;
    let tiles = tiles(&snap, &now_t, &hour);
    let host = machine::hostname();
    let machines = build_machines(&state, None, tz.day_start(now_ms())).await?;
    render(Overview {
        nav: nav(&state, &actor, "/machines").await,
        status: status(&host, &machines),
        host_sub: format!("AgentFlow's server · {} cores · {:.0} GB", snap.cores, snap.mem_total_gb()),
        host,
        tiles,
        machines,
    })
}
