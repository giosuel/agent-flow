//! The project document: one Markdown document per project, the agent's to shape (the owner,
//! 2026-11-12): rules for working on the project (like a per-project AGENTS.md), the tech stack and
//! platform, the roadmap if the project has one, pointers. `hello` returns it in full, so the agent
//! is never out of date; it stays small, and the agent changes one section at a time. The owner
//! reads and edits it on the project's Notes tab, laid out like the Guides. Every edit is a revision.

use anyhow::bail;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::records::event;
use crate::util::now_ms;

/// It's read at the start of every session: about 4,000 tokens at most.
pub const MAX_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Doc {
    pub text: String,
    pub rev: i64,
    pub updated_at: i64,
    pub author: String,
    /// What the last edit changed, e.g. "section Roadmap".
    pub what: String,
}

impl Doc {
    /// A short hash, so an edit made on an older version is noticed.
    pub fn version(&self) -> String {
        crate::util::hex(&Sha256::digest(self.text.as_bytes())[..6])
    }
}

pub fn get(conn: &Connection, project_id: i64) -> anyhow::Result<Doc> {
    Ok(conn
        .query_row(
            "SELECT text, rev, updated_at, author, what FROM project_docs WHERE project_id = ?1",
            [project_id],
            |r| Ok(Doc { text: r.get(0)?, rev: r.get(1)?, updated_at: r.get(2)?, author: r.get(3)?, what: r.get(4)? }),
        )
        .optional()?
        .unwrap_or_default())
}

/// Replaces the document, as a new revision. Returns it, and whether anything changed.
pub fn set(conn: &Connection, project_id: i64, text: &str, author: &str, what: &str) -> anyhow::Result<(Doc, bool)> {
    let text = crate::activity::redact(text.trim_end());
    let text = if text.is_empty() { String::new() } else { format!("{text}\n") };
    if text.len() > MAX_BYTES {
        bail!(
            "the project document would be {} KB; keep it under {} KB (it's read at the start of every session). \
             Move long material into notes and point to them",
            text.len() / 1024,
            MAX_BYTES / 1024
        );
    }
    let old = get(conn, project_id)?;
    if old.text == text {
        return Ok((old, false));
    }
    let rev = old.rev + 1;
    let now = now_ms();
    conn.execute(
        "INSERT INTO project_doc_revs (project_id, rev, at, author, what, text) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![project_id, rev, now, author, what, text],
    )?;
    conn.execute(
        "INSERT INTO project_docs (project_id, text, rev, updated_at, author, what) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (project_id) DO UPDATE SET text = ?2, rev = ?3, updated_at = ?4, author = ?5, what = ?6",
        params![project_id, text, rev, now, author, what],
    )?;
    event(conn, Some(project_id), None, "doc.updated", &format!("Project document: {what}"), author)?;
    Ok((get(conn, project_id)?, true))
}

/// The document's `##` sections: (title, text with its heading). Text before the first heading is
/// "Introduction".
pub fn sections(text: &str) -> Vec<(String, String)> {
    crate::guides::split(text).into_iter().map(|s| (s.title, s.text)).collect()
}

/// Replaces one `##` section (found by its title, any case), adds it at the end when there's
/// none, or removes it when `body` is empty.
pub fn edit_section(text: &str, title: &str, body: &str) -> anyhow::Result<String> {
    let title = title.trim().trim_start_matches('#').trim();
    if title.is_empty() || title.contains('\n') || title.chars().count() > 80 {
        bail!("a section is named by its heading, e.g. \"Roadmap\"");
    }
    let body = body.trim();
    let mut parts = sections(text);
    let found = parts.iter().position(|(t, _)| t.eq_ignore_ascii_case(title));
    // An existing section keeps its heading as written.
    let title = found.map(|i| parts[i].0.clone()).unwrap_or_else(|| title.to_owned());
    // A body that starts with its own heading keeps it; otherwise the heading is added.
    let new = if body.is_empty() {
        String::new()
    } else if body.starts_with("## ") {
        format!("{body}\n\n")
    } else {
        format!("## {title}\n\n{body}\n\n")
    };
    match found {
        Some(i) => parts[i].1 = new,
        None if body.is_empty() => bail!("there's no section {title:?} to remove"),
        None => {
            if let Some(last) = parts.last_mut()
                && !last.1.ends_with("\n\n")
            {
                last.1 = format!("{}\n\n", last.1.trim_end());
            }
            parts.push((title.clone(), new));
        }
    }
    Ok(parts.into_iter().map(|(_, t)| t).collect::<String>().trim_end().to_owned())
}

#[derive(Debug, Clone, Serialize)]
pub struct Rev {
    pub rev: i64,
    pub at: i64,
    pub author: String,
    pub what: String,
}

pub fn revisions(conn: &Connection, project_id: i64, limit: i64) -> anyhow::Result<Vec<Rev>> {
    let mut stmt = conn.prepare(
        "SELECT rev, at, author, what FROM project_doc_revs WHERE project_id = ?1 ORDER BY rev DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![project_id, limit], |r| {
        Ok(Rev { rev: r.get(0)?, at: r.get(1)?, author: r.get(2)?, what: r.get(3)? })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn revision_text(conn: &Connection, project_id: i64, rev: i64) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT text FROM project_doc_revs WHERE project_id = ?1 AND rev = ?2",
            params![project_id, rev],
            |r| r.get(0),
        )
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn sections_edit_and_revisions() {
        let doc = "Intro line.\n\n## How to work\n\n- Conventional Commits\n\n## Roadmap\n\n- M3 now\n";
        let d = edit_section(doc, "roadmap", "- M3 now\n- M4 next").unwrap();
        assert!(d.contains("## Roadmap\n\n- M3 now\n- M4 next") && d.contains("## How to work"), "{d}");
        let d = edit_section(&d, "Ports", "8990 thumbnails").unwrap();
        assert!(d.ends_with("## Ports\n\n8990 thumbnails"), "{d}");
        let d = edit_section(&d, "How to work", "").unwrap();
        assert!(!d.contains("How to work") && d.starts_with("Intro line."), "{d}");
        assert!(edit_section(&d, "Nope", "").is_err());
        assert!(edit_section(&d, "", "x").is_err());

        let db = Db::open_memory();
        db.call_blocking(|c| {
            let p = crate::projects::create(c, "lumen", "Lumen", "owner")?;
            assert_eq!(get(c, p.id)?.rev, 0);
            let (d1, changed) = set(c, p.id, "## Roadmap\n\n- M3", "lumen", "section Roadmap")?;
            assert!(changed && d1.rev == 1);
            let (_, changed) = set(c, p.id, "## Roadmap\n\n- M3\n", "lumen", "same")?;
            assert!(!changed);
            let (d2, _) = set(c, p.id, "## Roadmap\n\n- M4", "owner", "the whole document")?;
            assert_eq!(d2.rev, 2);
            assert_eq!(revisions(c, p.id, 10)?.len(), 2);
            assert_eq!(revision_text(c, p.id, 1)?.as_deref(), Some("## Roadmap\n\n- M3\n"));
            assert!(set(c, p.id, &"x".repeat(MAX_BYTES + 1), "lumen", "too much").is_err());
            Ok(())
        })
        .unwrap();
    }
}
