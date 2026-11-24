//! Nightly local backup and Markdown export (the owner, 2026-11-11: "backup local for now").
//!
//! Once a day AgentFlow copies its database to `<data_dir>/backups/flow-<date>.db` (SQLite's
//! VACUUM INTO: a consistent copy while it keeps running), keeping the last 30, and writes every
//! project as a Markdown file to `<data_dir>/export/<slug>.md`, readable without AgentFlow.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde_json::Value;

use crate::records::{self, Kind, Record};
use crate::util::fmt_utc;

pub const KEEP: usize = 30;

/// Today's backup, unless it exists already. Returns its path when one was made.
pub fn backup(conn: &Connection, data_dir: &Path, date: &str) -> anyhow::Result<Option<PathBuf>> {
    let dir = data_dir.join("backups");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("flow-{date}.db"));
    if path.exists() {
        return Ok(None);
    }
    copy_into(conn, &dir, &path)?;
    // Keep the newest daily ones; names sort by date.
    prune(&dir, |n| n.len() == "flow-2026-01-01.db".len(), KEEP);
    Ok(Some(path))
}

/// A backup on request (`agentflow backup`, e.g. before an install; the owner, 2026-11-17: "a
/// script (or a small server cli) that backs up the database that is shipped with agentflow"):
/// `backups/flow-<date>-<time>-<what>.db`, mode 600. The database is opened read-only, so the
/// program that makes it never migrates it (an install runs the new program before the old server
/// stops). The newest 20 of these are kept, apart from the daily ones.
pub fn backup_named(data_dir: &Path, db_path: &Path, what: &str) -> anyhow::Result<PathBuf> {
    let what: String = what
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '-' })
        .take(60)
        .collect();
    let conn = Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    let dir = data_dir.join("backups");
    std::fs::create_dir_all(&dir)?;
    let stamp = stamp();
    let path = dir.join(if what.is_empty() { format!("flow-{stamp}.db") } else { format!("flow-{stamp}-{what}.db") });
    copy_into(&conn, &dir, &path)?;
    prune(&dir, |n| n.len() > "flow-2026-01-01.db".len(), 20);
    Ok(path)
}

/// "2026-11-17-1230" in UTC.
fn stamp() -> String {
    let t = time::OffsetDateTime::from_unix_timestamp_nanos(crate::util::now_ms() as i128 * 1_000_000)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    t.format(time::macros::format_description!("[year]-[month]-[day]-[hour][minute]")).unwrap_or_default()
}

fn prune(dir: &Path, which: impl Fn(&str) -> bool, keep: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut all: Vec<PathBuf> = rd
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("flow-") && n.ends_with(".db") && which(n))
        })
        .collect();
    all.sort();
    let excess = all.len().saturating_sub(keep);
    for old in &all[..excess] {
        let _ = std::fs::remove_file(old);
    }
}

/// A consistent copy (SQLite's VACUUM INTO, fine while the server writes), mode 600.
fn copy_into(conn: &Connection, dir: &Path, path: &Path) -> anyhow::Result<()> {
    // The disk is shared with production: a copy needs room for itself and 5 GB to spare.
    let size: i64 =
        conn.query_row("SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()", [], |r| r.get(0))?;
    let free = rustix::fs::statvfs(dir).map(|s| s.f_bavail.saturating_mul(s.f_frsize)).unwrap_or(0);
    if free < size as u64 + 5 * 1024 * 1024 * 1024 {
        anyhow::bail!("not backing up: {} GB free on the disk is too little", free >> 30);
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("flow.db");
    let tmp = dir.join(format!(".{name}.tmp"));
    let _ = std::fs::remove_file(&tmp);
    conn.execute("VACUUM INTO ?1", [tmp.to_string_lossy()])?;
    // Only the server's user reads it, whatever the umask of whoever ran the backup.
    std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn record_md(out: &mut String, r: &Record) {
    let _ = writeln!(out, "### {} {} ({})\n", r.short_id(), r.title, r.state);
    let _ = writeln!(out, "_{} · {}_\n", r.author, fmt_utc(r.created_at));
    if !r.body.trim().is_empty() {
        let _ = writeln!(out, "{}\n", r.body.trim());
    }
    for (key, label) in [
        ("owner_words", "Owner's words"),
        ("answer_option_title", "Chosen"),
        ("answer", "Answer"),
        ("url", "Link"),
        ("tag", "Tag"),
        ("name", "Build"),
        ("estimate", "Estimate"),
    ] {
        let v = r.str(key);
        if !v.is_empty() {
            let _ = writeln!(out, "- {label}: {v}");
        }
    }
    if let Some(Value::Array(features)) = r.data.get("features") {
        for f in features {
            let _ = writeln!(out, "- Try: {}", f.get("title").and_then(Value::as_str).unwrap_or(""));
        }
    }
    out.push('\n');
}

/// One project as Markdown: its details, milestones and every record by kind.
pub fn project_md(conn: &Connection, p: &crate::projects::Project) -> anyhow::Result<String> {
    let mut out = String::new();
    let _ = writeln!(out, "# {}\n", p.name);
    if !p.description.is_empty() {
        let _ = writeln!(out, "{}\n", p.description);
    }
    for (label, v) in [("Repository", &p.repo), ("Checkout", &p.checkout), ("Status", &p.status)] {
        if !v.is_empty() {
            let _ = writeln!(out, "- {label}: {v}");
        }
    }
    out.push('\n');
    let ms = crate::projects::milestones(conn, p.id)?;
    if !ms.is_empty() {
        out.push_str("## Milestones\n\n");
        for m in &ms {
            let indent = if m.parent_id.is_some() { "  " } else { "" };
            let _ = writeln!(out, "{indent}- {} {} ({})", m.name, m.title, m.state);
        }
        out.push('\n');
    }
    let all = records::query(conn, &records::Query { project_id: Some(p.id), limit: 0, ..Default::default() })?;
    for kind in Kind::ALL {
        let mut list: Vec<&Record> = all.iter().filter(|r| r.kind == kind).collect();
        if list.is_empty() {
            continue;
        }
        list.sort_by_key(|r| r.number);
        let _ = writeln!(out, "## {}\n", kind.label());
        for r in list {
            record_md(&mut out, r);
        }
    }
    Ok(out)
}

/// Writes every project's Markdown; returns how many.
pub fn export(conn: &Connection, data_dir: &Path) -> anyhow::Result<usize> {
    let dir = data_dir.join("export");
    std::fs::create_dir_all(&dir)?;
    let list = crate::projects::list(conn)?;
    for p in &list {
        let tmp = dir.join(format!(".{}.md.tmp", p.slug));
        std::fs::write(&tmp, project_md(conn, p)?)?;
        std::fs::rename(&tmp, dir.join(format!("{}.md", p.slug)))?;
    }
    Ok(list.len())
}

/// Checks every hour whether today's backup exists, and makes it (with the export) if not.
pub fn spawn(state: crate::web::AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let data = state.config.data_dir();
            let date = crate::util::fmt_date(crate::util::now_ms());
            let result = state
                .db
                .call(move |c| {
                    let made = backup(c, &data, &date)?;
                    // Exported apart from the backup, so a failed export is tried again next hour.
                    let marker = data.join("export").join(".last");
                    let exported = if std::fs::read_to_string(&marker).ok().as_deref() != Some(date.as_str()) {
                        let n = export(c, &data)?;
                        std::fs::write(&marker, &date)?;
                        n
                    } else {
                        0
                    };
                    Ok((made, exported))
                })
                .await;
            match result {
                Ok((Some(path), n)) => tracing::info!("backed up to {} and exported {n} projects", path.display()),
                Ok((None, n)) if n > 0 => tracing::info!("exported {n} projects"),
                Ok((None, _)) => {}
                Err(e) => tracing::warn!("backup: {e:#}"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_named_backup_is_read_only_private_and_kept_apart_from_the_daily_ones() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("flow.db");
        drop(crate::db::Db::open(&db_path).unwrap());
        let a = backup_named(dir.path(), &db_path, "pre install/../x").unwrap();
        let name = a.file_name().unwrap().to_str().unwrap().to_owned();
        assert!(name.starts_with("flow-") && name.ends_with("-pre-install-..-x.db") && !name.contains('/'), "{name}");
        assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o600);
        let copy = Connection::open(&a).unwrap();
        let n: i64 =
            copy.query_row("SELECT count(*) FROM sqlite_master WHERE name = 'projects'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "a whole database");
    }

    #[test]
    fn backups_and_exports() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flow.db");
        let db = crate::db::Db::open(&path).unwrap();
        let d = dir.path().to_owned();
        db.call_blocking(move |c| {
            let p = crate::projects::create(c, "vinculum", "Vinculum", "t")?;
            records::create(
                c,
                p.id,
                "vinculum",
                records::NewRecord {
                    kind: Some(Kind::Decision),
                    title: "Tabs on the left".into(),
                    body: "Fits 11 categories.".into(),
                    ..Default::default()
                },
            )?;
            let made = backup(c, &d, "2026-11-11")?.unwrap();
            assert!(backup(c, &d, "2026-11-11")?.is_none(), "once a day");
            let copy = Connection::open(&made)?;
            let n: i64 = copy.query_row("SELECT count(*) FROM records", [], |r| r.get(0))?;
            assert_eq!(n, 1);
            for day in 1..=KEEP + 2 {
                backup(c, &d, &format!("2026-10-{day:02}"))?;
            }
            assert_eq!(std::fs::read_dir(d.join("backups"))?.count(), KEEP);
            assert!(d.join("backups/flow-2026-11-11.db").exists(), "the newest stay");
            assert_eq!(export(c, &d)?, 1);
            let md = std::fs::read_to_string(d.join("export/vinculum.md"))?;
            assert!(
                md.starts_with("# Vinculum") && md.contains("Tabs on the left (current)") && md.contains("Fits 11")
            );
            Ok(())
        })
        .unwrap();
    }
}
