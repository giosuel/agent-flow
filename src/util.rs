//! Small helpers shared by the server and the CLI: time, random secrets and hashing.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;
use sha2::{Digest, Sha256};

/// Milliseconds since the Unix epoch, UTC. Every timestamp in the database uses this.
pub fn now_ms() -> i64 {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    d.as_millis() as i64
}

/// Locks a std mutex, also after a panic elsewhere poisoned it: what's kept behind these (caches,
/// counters, pending ceremonies) is still usable, and one panic shouldn't take every page down.
pub fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Lowercase hex, two digits a byte.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A random secret with a readable prefix, e.g. `af_dev_…`. 32 random bytes, so guessing one is hopeless.
pub fn new_secret(prefix: &str) -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// Secrets are stored only as their SHA-256 hash. They are long random values, so a plain hash is
/// enough; there is nothing for a slow password hash to protect.
pub fn hash_secret(secret: &str) -> Vec<u8> {
    Sha256::digest(secret.as_bytes()).to_vec()
}

/// "3 min ago", "yesterday", "5 days ago": the relative times the page shows.
pub fn ago(then_ms: i64, now_ms: i64) -> String {
    let s = (now_ms - then_ms).max(0) / 1000;
    match s {
        0..=44 => "just now".into(),
        45..=89 => "1 min ago".into(),
        90..=3599 => format!("{} min ago", (s + 30) / 60),
        3600..=5399 => "1 hour ago".into(),
        5400..=86399 => format!("{} hours ago", (s + 1800) / 3600),
        86400..=172799 => "yesterday".into(),
        _ => format!("{} days ago", s / 86400),
    }
}

/// RFC 3339 UTC, "2026-11-11T01:30:05Z": what agents, the API, the CLI and exports get. Pages
/// show the owner's own time zone instead (see [`crate::tz`]).
pub fn fmt_utc(ms: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let f = time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
    t.format(&f).unwrap_or_default()
}

/// "35,948": counts with thousands separators.
pub fn fmt_count(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, d) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(d);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// "02:04" (UTC).
pub fn fmt_hm(ms: i64) -> String {
    fmt_utc(ms).get(11..16).unwrap_or("").to_owned()
}

/// "2026-11-06" (UTC).
pub fn fmt_date(ms: i64) -> String {
    let t = time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    let f = time::macros::format_description!("[year]-[month]-[day]");
    t.format(&f).unwrap_or_default()
}

/// Expands a leading `~/` to the home directory.
pub fn expand_home(path: &str) -> std::path::PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home_dir().join(rest),
        None => path.into(),
    }
}

static AGENTS_HOME: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// The agents' home folder, where `~/` in paths points: $HOME, unless the server runs as its own
/// user and the config names the agents' home (`agents_home`).
pub fn home_dir() -> std::path::PathBuf {
    if let Some(h) = AGENTS_HOME.get() {
        return h.clone();
    }
    std::env::var_os("HOME").map(Into::into).unwrap_or_else(|| "/".into())
}

/// A user's id by name, from /etc/passwd ("agentflow" → 997), or a plain number.
pub fn uid_of(user: &str) -> anyhow::Result<u32> {
    if let Ok(n) = user.parse() {
        return Ok(n);
    }
    let passwd = std::fs::read_to_string("/etc/passwd")?;
    passwd
        .lines()
        .filter_map(|l| {
            let mut f = l.split(':');
            Some((f.next()?, f.nth(1)?))
        })
        .find(|(name, _)| *name == user)
        .and_then(|(_, uid)| uid.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("no user {user:?} on this machine"))
}

pub fn set_agents_home(dir: std::path::PathBuf) {
    let _ = AGENTS_HOME.set(dir);
}

/// "2026-11-06" or "2026-11-06 14:12" (UTC) → milliseconds.
pub fn parse_day_or_time(s: &str) -> anyhow::Result<i64> {
    let s = s.trim();
    let f = time::macros::format_description!("[year]-[month]-[day]");
    let fm = time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]");
    if let Ok(d) = time::PrimitiveDateTime::parse(s, &fm) {
        return Ok((d.assume_utc().unix_timestamp_nanos() / 1_000_000) as i64);
    }
    let d = time::Date::parse(s, &f).map_err(|_| anyhow::anyhow!("dates look like 2026-11-06 or 2026-11-06 14:12"))?;
    Ok((d.midnight().assume_utc().unix_timestamp_nanos() / 1_000_000) as i64)
}

#[cfg(test)]
mod tests {
    #[test]
    fn counts_get_separators() {
        assert_eq!(super::fmt_count(35_948), "35,948");
        assert_eq!(super::fmt_count(999), "999");
        assert_eq!(super::fmt_count(-1_234_567), "-1,234,567");
    }

    use super::*;

    #[test]
    fn secrets_are_unique_and_prefixed() {
        let a = new_secret("af_dev_");
        let b = new_secret("af_dev_");
        assert!(a.starts_with("af_dev_"));
        assert_ne!(a, b);
        assert_eq!(hash_secret(&a).len(), 32);
    }

    #[test]
    fn relative_times() {
        let now = 1_000_000_000_000;
        assert_eq!(ago(now - 10_000, now), "just now");
        assert_eq!(ago(now - 5 * 60_000, now), "5 min ago");
        assert_eq!(ago(now - 3 * 3_600_000, now), "3 hours ago");
        assert_eq!(ago(now - 30 * 3_600_000, now), "yesterday");
        assert_eq!(ago(now - 5 * 86_400_000, now), "5 days ago");
    }
}
