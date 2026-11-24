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
    // The disk is shared with production: a copy needs room for itself and 5 GB to spare.
    let size: i64 =
        conn.query_row("SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()", [], |r| r.get(0))?;
    let free = rustix::fs::statvfs(&dir).map(|s| s.f_bavail.saturating_mul(s.f_frsize)).unwrap_or(0);
    if free < size as u64 + 5 * 1024 * 1024 * 1024 {
        anyhow::bail!("not backing up: {} GB free on the disk is too little", free >> 30);
    }
    let tmp = dir.join(format!(".flow-{date}.db.tmp"));
    let _ = std::fs::remove_file(&tmp);
    conn.execute("VACUUM INTO ?1", [tmp.to_string_lossy()])?;
    // Only the server's user reads it, whatever the umask of whoever ran the backup.
    std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    std::fs::rename(&tmp, &path)?;
    // Keep the newest ones; names sort by date.
    let mut all: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("flow-") && n.ends_with(".db"))
        })
        .collect();
    all.sort();
    let excess = all.len().saturating_sub(KEEP);
    for old in &all[..excess] {
        let _ = std::fs::remove_file(old);
    }
    Ok(Some(path))
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
