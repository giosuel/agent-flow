//! The Dashboard's overview tiles (the owner, 2026-11-18: "lets add some more stuff to the
//! dashboard, like some quick overview. I think maybe something about the machine and usage would
//! be good, a quick recap of today's usage maybe"; layout A, D-212): the plan's windows, today's
//! usage, the machine, and the agents and build servers. Each tile opens its page.

use rusqlite::Connection;

use crate::plan::{FIVE_HOURS, WEEK};
use crate::prices::dollars;
use crate::tz::Tz;
use crate::usage::{self, Tokens};

/// A meter: how full (0–100) and, for the plan, how much of the window's time has passed.
pub struct Meter {
    pub pct: f64,
    pub mark: Option<f64>,
    pub warn: bool,
    pub label: String,
    pub value: String,
}

pub struct ProjectBar {
    pub name: String,
    pub usd: String,
    pub width: f64,
    pub tint: &'static str,
}

pub struct Builder {
    pub name: String,
    pub online: bool,
    pub job: String,
}

pub struct Overview {
    /// The plan's two windows; empty before a status line sent any.
    pub plan: Vec<Meter>,
    pub plan_note: String,
    /// The phone's plan tile: "42%", "week 71% · resets 22:50".
    pub plan_big: String,
    pub plan_sub: String,
    pub today_usd: String,
    /// "a day ≈ $56 lately", or empty.
    pub typical: String,
    pub new_tokens: String,
    pub calls: String,
    pub cache_reads: String,
    pub top: Vec<ProjectBar>,
    pub machine: Vec<Meter>,
    pub cpu_short: String,
    pub mem_short: String,
    pub online: usize,
    pub agents: usize,
    pub builders: Vec<Builder>,
    /// CPU over the last hour, as bar heights in %.
    pub spark: Vec<u32>,
    /// Busy cores in the last minute's reading.
    busy: Option<f64>,
}

fn sum(rows: &[(Option<i64>, i64, Tokens)]) -> Tokens {
    let mut t = Tokens::default();
    for (_, _, r) in rows {
        t.add(r);
    }
    t
}

/// Everything from the database; the machine's own numbers are added by [`with_machine`].
pub fn load(c: &Connection, now: i64, tz: &Tz, online: usize, agents: usize) -> anyhow::Result<Overview> {
    // The plan's windows, with where their time is.
    let mut plan = Vec::new();
    let (mut plan_big, mut plan_sub) = (String::new(), String::new());
    if let Some(s) = crate::plan::latest(c)? {
        for (label, pct, reset, len) in
            [("5 hours", s.five_pct, s.five_reset, FIVE_HOURS), ("Week", s.week_pct, s.week_reset, WEEK)]
        {
            let (Some(pct), Some(reset)) = (pct, reset) else { continue };
            let live = reset > now;
            let pct = if live { pct } else { 0.0 };
            let mark = live.then(|| ((now - (reset - len)) as f64 / len as f64 * 100.0).clamp(0.0, 100.0));
            let when = if !live {
                "reset".to_owned()
            } else if len == FIVE_HOURS {
                tz.hm(reset)
            } else {
                format!("{} {}", tz.day(reset), tz.hm(reset))
            };
            plan.push(Meter {
                warn: pct >= 80.0 || mark.is_some_and(|m| pct > m + 5.0),
                pct,
                mark,
                label: label.into(),
                value: format!("{pct:.0}% · {when}"),
            });
        }
        if let Some(f) = plan.first() {
            plan_big = format!("{:.0}%", f.pct);
            plan_sub = plan
                .get(1)
                .map(|w| format!("week {:.0}% · resets {}", w.pct, f.value.split(" · ").nth(1).unwrap_or("")))
                .unwrap_or_default();
        }
    }
    let plan_note = if plan.is_empty() {
        "No status line has sent the plan's usage yet.".into()
    } else {
        "The yellow mark is how much of the window's time has passed.".into()
    };

    // Today, from midnight in the owner's time zone (D-212), and a usual day of the last two weeks.
    let days = tz.days(now, 15);
    let (today_from, end) = (days[14], days[15]);
    let rows = usage::per_project(c, today_from, end)?;
    let today = sum(&rows);
    let mut daily: Vec<f64> = Vec::new();
    for w in days[..15].windows(2) {
        let d = sum(&usage::per_project(c, w[0], w[1])?).usd();
        if d > 0.0 {
            daily.push(d);
        }
    }
    let typical = crate::plan::median(daily).map(|d| format!("a day ≈ {} lately", dollars(d))).unwrap_or_default();
    let projects = crate::projects::list(c)?;
    // Each project keeps its colour from Usage and Machine (charts::colors: by project, never by
    // rank; N-20 #5: it took its rank's colour here).
    let colors = super::charts::colors(projects.iter().map(|p| (p.id, p.slug.as_str())));
    let mut per: Vec<(String, &'static str, f64)> = rows
        .iter()
        .filter_map(|(p, _, t)| {
            let (name, tint) = match projects.iter().position(|x| Some(x.id) == *p) {
                Some(i) => (projects[i].name.clone(), colors[i]),
                None => ("no project".into(), super::charts::OTHER),
            };
            Some((name, tint, t.usd())).filter(|(_, _, u)| *u > 0.0)
        })
        .collect();
    per.sort_by(|a, b| b.2.total_cmp(&a.2));
    let top_usd = per.first().map(|x| x.2).unwrap_or(1.0).max(0.01);
    let top = per
        .into_iter()
        .take(3)
        .map(|(name, tint, u)| ProjectBar { name, usd: dollars(u), width: (u / top_usd * 100.0).max(2.0), tint })
        .collect();

    // Build servers: online or not, and the job each runs.
    let mut builders = Vec::new();
    for b in crate::remote::load_servers(c)? {
        let job: Option<(String, String)> = c
            .query_row(
                "SELECT j.command, p.slug FROM remote_jobs j JOIN projects p ON p.id = j.project_id
                 WHERE j.server = ?1 AND j.state = 'running' ORDER BY j.id DESC LIMIT 1",
                [&b.name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let waiting: i64 = c.query_row(
            "SELECT count(*) FROM remote_jobs WHERE server = ?1 AND state IN ('queued', 'running', 'paused')",
            [&b.name],
            |r| r.get(0),
        )?;
        let online = crate::builders::hub().is_online(&b.name);
        let job = match job {
            Some((cmd, slug)) => {
                let argv: Vec<String> = serde_json::from_str(&cmd).unwrap_or_default();
                let shown: String = argv.iter().take(3).cloned().collect::<Vec<_>>().join(" ");
                format!("{waiting} job{} · {shown}, {slug}", if waiting == 1 { "" } else { "s" })
            }
            None if waiting > 0 => format!("{waiting} waiting"),
            None if online => "idle".into(),
            None => "offline".into(),
        };
        builders.push(Builder { name: b.name, online, job });
    }

    // CPU over the last hour: 24 bars of 2.5 minutes.
    let mins = crate::machine::totals_since(c, now - 3_600_000)?;
    let mut buckets = [0.0f64; 24];
    for t in &mins {
        let i = (((t.at - (now - 3_600_000)) / 150_000) as usize).min(23);
        buckets[i] = buckets[i].max(t.cpu);
    }
    let peak = buckets.iter().cloned().fold(0.0, f64::max).max(0.5);
    let spark = if mins.is_empty() {
        Vec::new()
    } else {
        buckets.iter().map(|b| ((b / peak) * 100.0).round() as u32).collect()
    };

    Ok(Overview {
        plan,
        plan_note,
        plan_big,
        plan_sub,
        today_usd: dollars(today.usd()),
        typical,
        new_tokens: usage::short(today.new_tokens()),
        calls: today.calls.to_string(),
        cache_reads: usage::short(today.cache_read),
        top,
        machine: Vec::new(),
        cpu_short: String::new(),
        mem_short: String::new(),
        online,
        agents,
        builders,
        spark,
        busy: mins.last().map(|t| t.cpu),
    })
}

/// The machine's meters, read now: busy cores (the last minute's reading), memory, disk, /tmp.
pub fn with_machine(mut o: Overview) -> Overview {
    let s = crate::machine::snapshot();
    let cpu = o.busy.unwrap_or(s.load1);
    let gb = |b: u64| b as f64 / 1e9;
    let (disk_free, disk_total) = crate::machine::disk("/");
    let (tmp_free, tmp_total) = crate::machine::disk("/tmp");
    let used = |free: u64, total: u64| if total == 0 { 0.0 } else { (1.0 - free as f64 / total as f64) * 100.0 };
    o.machine = vec![
        Meter {
            pct: (cpu / s.cores.max(1) as f64 * 100.0).min(100.0),
            mark: None,
            warn: cpu > s.cores as f64 * 0.85,
            label: "CPU".into(),
            value: format!("{cpu:.1} of {}", s.cores),
        },
        Meter {
            pct: s.mem_used_gb() / s.mem_total_gb().max(0.1) * 100.0,
            mark: None,
            warn: s.mem_used_gb() > s.mem_total_gb() * 0.85,
            label: "Memory".into(),
            value: format!("{:.1} of {:.0} GB", s.mem_used_gb(), s.mem_total_gb()),
        },
        Meter {
            pct: used(disk_free, disk_total),
            mark: None,
            warn: used(disk_free, disk_total) >= 80.0,
            label: "Disk".into(),
            value: format!("{:.0} GB free", gb(disk_free)),
        },
        Meter {
            pct: used(tmp_free, tmp_total),
            mark: None,
            warn: used(tmp_free, tmp_total) >= 80.0,
            label: "/tmp".into(),
            value: format!("{:.1} GB free", gb(tmp_free)),
        },
    ];
    o.cpu_short = format!("{cpu:.1} / {}", s.cores);
    o.mem_short = format!("cores · {:.1} of {:.0} GB", s.mem_used_gb(), s.mem_total_gb());
    o
}
