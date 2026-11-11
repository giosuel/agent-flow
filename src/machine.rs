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

#[cfg(test)]
mod tests {
    #[test]
    fn reads_proc() {
        let s = super::snapshot();
        assert!(s.cores >= 1);
        assert!(s.mem_total_kb > 0 && s.mem_available_kb <= s.mem_total_kb);
        assert!(s.summary().contains(" GB"));
    }
}
