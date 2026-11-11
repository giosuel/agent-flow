//! This machine's load and memory, read from /proc. AgentFlow runs on one machine, so this is
//! the machine every local agent shares.

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub cores: usize,
    /// One-minute load average: roughly how many cores are busy.
    pub load1: f64,
    pub mem_total_kb: u64,
    pub mem_available_kb: u64,
}

impl Snapshot {
    pub fn mem_used_gb(&self) -> f64 {
        (self.mem_total_kb.saturating_sub(self.mem_available_kb)) as f64 / 1024.0 / 1024.0
    }

    pub fn mem_total_gb(&self) -> f64 {
        self.mem_total_kb as f64 / 1024.0 / 1024.0
    }

    /// "3.1 of 8 cores, 9.4 of 22 GB"
    pub fn summary(&self) -> String {
        format!("{:.1} of {} cores, {:.1} of {:.0} GB", self.load1, self.cores, self.mem_used_gb(), self.mem_total_gb())
    }
}

pub fn snapshot() -> Snapshot {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let load1 = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse().ok()))
        .unwrap_or(0.0);
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let field = |name: &str| -> u64 {
        meminfo
            .lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    Snapshot { cores, load1, mem_total_kb: field("MemTotal:"), mem_available_kb: field("MemAvailable:") }
}

/// This machine's name, e.g. "giosuelio".
pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim().to_owned()
}

/// Load averages over 1, 5 and 15 minutes.
pub fn loads() -> (f64, f64, f64) {
    let s = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let mut it = s.split_whitespace().map(|v| v.parse().unwrap_or(0.0));
    (it.next().unwrap_or(0.0), it.next().unwrap_or(0.0), it.next().unwrap_or(0.0))
}

/// CPU time in jiffies from /proc/stat's first line: (busy, total).
fn cpu_jiffies() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let nums: Vec<u64> = stat.lines().next()?.split_whitespace().skip(1).filter_map(|v| v.parse().ok()).collect();
    // user nice system idle iowait irq softirq steal (guest time is already in user).
    let total: u64 = nums.iter().take(8).sum();
    let idle = nums.get(3).copied().unwrap_or(0) + nums.get(4).copied().unwrap_or(0);
    Some((total - idle, total))
}

static LAST_CPU: std::sync::Mutex<Option<(std::time::Instant, u64, u64)>> = std::sync::Mutex::new(None);

/// How many cores are busy right now: measured since the last call when that was between 0.5
/// and 60 seconds ago (the Machine page asks every 5), otherwise over a quarter second.
pub async fn busy_cores(cores: usize) -> f64 {
    let fresh = |at: std::time::Instant| (0.5..60.0).contains(&at.elapsed().as_secs_f64());
    let prev = *LAST_CPU.lock().unwrap_or_else(|e| e.into_inner());
    let (b0, t0) = match prev {
        Some((at, b, t)) if fresh(at) => (b, t),
        _ => {
            let Some((b, t)) = cpu_jiffies() else { return 0.0 };
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            (b, t)
        }
    };
    let Some((b1, t1)) = cpu_jiffies() else { return 0.0 };
    *LAST_CPU.lock().unwrap_or_else(|e| e.into_inner()) = Some((std::time::Instant::now(), b1, t1));
    if t1 <= t0 {
        return 0.0;
    }
    (b1.saturating_sub(b0)) as f64 / (t1 - t0) as f64 * cores as f64
}

/// Free and total bytes of the filesystem holding `path`.
pub fn disk(path: &str) -> (u64, u64) {
    match rustix::fs::statvfs(path) {
        Ok(s) => (s.f_bavail.saturating_mul(s.f_frsize), s.f_blocks.saturating_mul(s.f_frsize)),
        Err(_) => (0, 0),
    }
}

// ---------------------------------------------------------------------------------------------
// Who uses the machine: every process, grouped by the agent session that started it.

const TICKS: f64 = 100.0; // USER_HZ on Linux
const PAGE_KB: u64 = 4;

#[derive(Debug, Clone)]
struct Proc {
    ppid: i32,
    comm: String,
    ticks: u64,
    rss_kb: u64,
}

/// /proc/<pid>/stat: the name is in parentheses and may hold spaces, so fields count from the
/// last ')'.
fn parse_stat(text: &str) -> Option<Proc> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = text[open + 1..close].to_owned();
    let f: Vec<&str> = text[close + 1..].split_whitespace().collect();
    // After ')': state(0) ppid(1) … utime(11) stime(12) … rss(21)
    Some(Proc {
        ppid: f.get(1)?.parse().ok()?,
        comm,
        ticks: f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?,
        rss_kb: f.get(21)?.parse::<u64>().ok()? * PAGE_KB,
    })
}

fn processes() -> std::collections::HashMap<i32, Proc> {
    let mut out = std::collections::HashMap::new();
    let Ok(list) = std::fs::read_dir("/proc") else { return out };
    for e in list.filter_map(Result::ok) {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        if let Some(p) = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok().and_then(|t| parse_stat(&t)) {
            out.insert(pid, p);
        }
    }
    out
}

/// One group's share of a minute: cores busy on average, memory now, and its busiest command.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Share {
    pub cores: f64,
    pub mem_kb: u64,
    pub top: String,
}

/// Keeps the previous reading, so CPU is the work done between two samples.
pub struct Sampler {
    last: std::collections::HashMap<i32, u64>,
    last_at: std::time::Instant,
}

impl Default for Sampler {
    fn default() -> Self {
        Self { last: Default::default(), last_at: std::time::Instant::now() }
    }
}

impl Sampler {
    /// Shares per group: a project's slug for a Claude session started in its checkout (and
    /// everything below it), "other" for the rest, and "machine" for the whole machine.
    /// `checkouts` maps a checkout directory to its project's slug.
    pub fn sample(&mut self, checkouts: &[(std::path::PathBuf, String)]) -> Vec<(String, Share)> {
        let procs = processes();
        let secs = self.last_at.elapsed().as_secs_f64().max(0.001);
        let first = self.last.is_empty();
        // Which group each Claude session belongs to.
        let mut roots: std::collections::HashMap<i32, String> = Default::default();
        // Another user's working folder isn't readable from /proc: ask as the agents' user.
        let sessions: Vec<i32> = procs.iter().filter(|(_, p)| p.comm == "claude").map(|(pid, _)| *pid).collect();
        let cwds: Vec<(i32, std::path::PathBuf)> =
            crate::hands::run(crate::hands::Job::Cwds { pids: sessions }).unwrap_or_default();
        for (pid, cwd) in &cwds {
            if let Some((_, slug)) =
                checkouts.iter().filter(|(dir, _)| cwd.starts_with(dir)).max_by_key(|(d, _)| d.as_os_str().len())
            {
                roots.insert(*pid, slug.clone());
            }
        }
        let group_of = |mut pid: i32| -> String {
            for _ in 0..64 {
                if let Some(g) = roots.get(&pid) {
                    return g.clone();
                }
                match procs.get(&pid) {
                    Some(p) if p.ppid > 1 => pid = p.ppid,
                    _ => break,
                }
            }
            "other".into()
        };
        let mut shares: std::collections::HashMap<String, (Share, f64)> = Default::default();
        let mut machine = Share::default();
        for (pid, p) in &procs {
            let busy = if first {
                0.0
            } else {
                p.ticks.saturating_sub(*self.last.get(pid).unwrap_or(&p.ticks)) as f64 / TICKS / secs
            };
            let g = group_of(*pid);
            let e = shares.entry(g).or_default();
            e.0.cores += busy;
            e.0.mem_kb += p.rss_kb;
            if busy > e.1 {
                e.1 = busy;
                e.0.top = p.comm.clone();
            }
            machine.cores += busy;
        }
        let snap = snapshot();
        machine.mem_kb = snap.mem_total_kb.saturating_sub(snap.mem_available_kb);
        self.last = procs.iter().map(|(pid, p)| (*pid, p.ticks)).collect();
        self.last_at = std::time::Instant::now();
        let mut out: Vec<(String, Share)> = shares.into_iter().map(|(g, (s, _))| (g, s)).collect();
        out.push(("machine".into(), machine));
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

/// Keeps a week of samples.
pub const KEEP_MS: i64 = 7 * 24 * 3_600_000;

/// Samples every minute into `machine_samples` (the first reading only sets the baseline).
pub fn spawn_sampler(state: crate::web::AppState) {
    tokio::spawn(async move {
        let mut sampler = Sampler::default();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut first = true;
        loop {
            tick.tick().await;
            let checkouts = match state.db.call(|c| checkout_dirs(c)).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("machine sample: {e:#}");
                    continue;
                }
            };
            let (s, shares) = tokio::task::spawn_blocking(move || {
                let shares = sampler.sample(&checkouts);
                (sampler, shares)
            })
            .await
            .unwrap_or_else(|_| (Sampler::default(), Vec::new()));
            sampler = s;
            if std::mem::take(&mut first) {
                continue;
            }
            let now = crate::util::now_ms();
            if let Err(e) = state.db.call(move |c| store(c, now, &shares)).await {
                tracing::warn!("machine sample: {e:#}");
            }
        }
    });
}

/// Each project's checkout, expanded, with its slug.
pub fn checkout_dirs(c: &rusqlite::Connection) -> anyhow::Result<Vec<(std::path::PathBuf, String)>> {
    Ok(crate::projects::list(c)?
        .into_iter()
        .filter(|p| !p.checkout.trim().is_empty())
        .map(|p| (crate::util::expand_home(p.checkout.trim()), p.slug))
        .collect())
}

pub fn store(c: &rusqlite::Connection, at: i64, shares: &[(String, Share)]) -> anyhow::Result<()> {
    for (g, s) in shares {
        c.execute(
            "INSERT INTO machine_samples (at, grp, cores, mem_kb, top) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![at, g, s.cores, s.mem_kb as i64, s.top],
        )?;
    }
    c.execute("DELETE FROM machine_samples WHERE at < ?1", [at - KEEP_MS])?;
    Ok(())
}

/// Average cores and memory per (bucket, group) from `from`, buckets of `step` ms. A group has
/// rows only in minutes it had processes, so the average is over every sample of the bucket:
/// an agent busy for one minute of fifteen counts a fifteenth.
pub fn history(c: &rusqlite::Connection, from: i64, step: i64) -> anyhow::Result<Vec<(i64, String, f64, f64)>> {
    let mut stmt = c.prepare(
        "WITH s AS (SELECT (at - ?1) / ?2 AS b, at, grp, cores, mem_kb FROM machine_samples WHERE at >= ?1),
              n AS (SELECT b, count(DISTINCT at) AS k FROM s GROUP BY b)
         SELECT s.b, s.grp, sum(s.cores) / n.k, sum(s.mem_kb) * 1.0 / n.k FROM s JOIN n USING (b)
         GROUP BY s.b, s.grp ORDER BY s.b",
    )?;
    let rows = stmt.query_map(rusqlite::params![from, step], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// A group's day: (group, CPU-hours, peak memory in KB, cores in the newest sample, its busiest command).
pub type GroupDay = (String, f64, i64, f64, String);

/// Per group since `from`.
pub fn group_summary(c: &rusqlite::Connection, from: i64) -> anyhow::Result<Vec<GroupDay>> {
    let mut stmt = c.prepare(
        "SELECT grp, sum(cores) / 60.0, max(mem_kb),
                (SELECT cores FROM machine_samples s2 WHERE s2.grp = s.grp ORDER BY at DESC LIMIT 1),
                (SELECT top FROM machine_samples s2 WHERE s2.grp = s.grp ORDER BY at DESC LIMIT 1)
         FROM machine_samples s WHERE at >= ?1 GROUP BY grp",
    )?;
    let rows = stmt.query_map([from], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_proc() {
        let s = super::snapshot();
        assert!(s.cores >= 1);
        assert!(s.mem_total_kb > 0 && s.mem_available_kb <= s.mem_total_kb);
        assert!(s.summary().contains(" GB"));
    }

    #[test]
    fn stat_lines_with_odd_names() {
        let p = super::parse_stat("4242 (tokio (rt) x) S 7 4242 4242 0 -1 4194304 1 0 0 0 150 50 0 0 20 0 9 0 100 123456 2500 18446744073709551615").unwrap();
        assert_eq!((p.ppid, p.comm.as_str(), p.ticks, p.rss_kb), (7, "tokio (rt) x", 200, 10_000));
    }

    #[test]
    fn history_averages_over_every_sample() {
        let db = crate::db::Db::open_memory();
        db.call_blocking(|c| {
            let share = |cores: f64| super::Share { cores, mem_kb: 1024, top: String::new() };
            // Three minutes; the agent only has processes in the first.
            super::store(c, 0, &[("machine".into(), share(1.0)), ("vinculum".into(), share(3.0))])?;
            super::store(c, 60_000, &[("machine".into(), share(1.0))])?;
            super::store(c, 120_000, &[("machine".into(), share(1.0))])?;
            let h = super::history(c, 0, 180_000)?;
            let v = h.iter().find(|r| r.1 == "vinculum").unwrap();
            assert!((v.2 - 1.0).abs() < 1e-9, "3 cores for a third of the bucket: {v:?}");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn this_process_is_sampled() {
        let mut s = super::Sampler::default();
        let first = s.sample(&[]);
        assert!(first.iter().any(|(g, _)| g == "machine"));
        let again = s.sample(&[(std::path::PathBuf::from("/"), "everything".into())]);
        let mem: u64 = again.iter().filter(|(g, _)| g != "machine").map(|(_, x)| x.mem_kb).sum();
        assert!(mem > 0);
    }
}
