//! The project document: one Markdown document per project, the agent's to shape (the owner,
//! 2026-11-12): rules for working on the project (like a per-project AGENTS.md), the tech stack and
//! platform, the roadmap if the project has one, pointers. `hello` returns it in full, so the agent
//! is never out of date; it stays small, and the agent changes one section at a time. The owner
//! reads it in a dialog from the project's overview and edits it as one Markdown document (the
//! owner, 2026-11-18: "just the whole document"). Every edit is a revision.

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
    crate::handbook::split(text).into_iter().map(|s| (s.title, s.text)).collect()
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

/// A section both sides changed since the version the owner started from.
#[derive(Debug, Clone, PartialEq)]
pub struct Conflict {
    pub title: String,
    /// The agent's version, heading and all; empty when it removed the section.
    pub theirs: String,
    /// Whether the owner's text has the section (false: the owner removed it).
    pub mine_kept: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Merged {
    pub text: String,
    /// Sections the other side changed that were taken over.
    pub taken: Vec<String>,
    pub conflicts: Vec<Conflict>,
}

/// A section's key: its title in lower case, and which of that name it is.
fn keyed(text: &str) -> Vec<((String, usize), String, String)> {
    let mut seen: Vec<String> = Vec::new();
    sections(text)
        .into_iter()
        .map(|(title, body)| {
            let k = title.to_lowercase();
            let n = seen.iter().filter(|s| **s == k).count();
            seen.push(k.clone());
            ((k, n), title, body)
        })
        .collect()
}

fn same(a: Option<&str>, b: Option<&str>) -> bool {
    a.map(str::trim_end) == b.map(str::trim_end)
}

fn join(parts: &[String]) -> String {
    let mut out = String::new();
    for p in parts {
        out.push_str(p.trim_end());
        out.push_str("\n\n");
    }
    out.trim_end().to_owned()
}

/// The owner's edit of `base` merged with the agent's edit of it, by `##` section (the owner,
/// 2026-11-18: keep the owner's edits, take the agent's changes to the sections the owner didn't
/// touch, and let the owner pick where both changed one). A conflict keeps the owner's version.
pub fn merge(base: &str, theirs: &str, mine: &str) -> Merged {
    let (b, t, m) = (keyed(base), keyed(theirs), keyed(mine));
    let find = |v: &[((String, usize), String, String)], k: &(String, usize)| {
        v.iter().find(|x| &x.0 == k).map(|x| x.2.clone())
    };
    let mut out: Vec<((String, usize), String)> = Vec::new();
    let mut taken = Vec::new();
    let mut conflicts = Vec::new();
    for (k, title, mb) in &m {
        let (bb, tb) = (find(&b, k), find(&t, k));
        if same(tb.as_deref(), bb.as_deref()) || same(tb.as_deref(), Some(mb)) {
            out.push((k.clone(), mb.clone()));
        } else if same(Some(mb), bb.as_deref()) {
            // Only the agent changed it (or removed it).
            taken.push(title.clone());
            if let Some(tb) = tb {
                out.push((k.clone(), tb));
            }
        } else {
            conflicts.push(Conflict { title: title.clone(), theirs: tb.unwrap_or_default(), mine_kept: true });
            out.push((k.clone(), mb.clone()));
        }
    }
    // Sections only the agent has: added by it, or removed by the owner.
    for (i, (k, title, tb)) in t.iter().enumerate() {
        if m.iter().any(|x| &x.0 == k) {
            continue;
        }
        match find(&b, k) {
            // The owner removed it and the agent didn't touch it: it stays removed.
            Some(bb) if same(Some(&bb), Some(tb)) => {}
            Some(_) => conflicts.push(Conflict { title: title.clone(), theirs: tb.clone(), mine_kept: false }),
            None => {
                taken.push(title.clone());
                // After the section it follows in the agent's version, or at the end.
                let at = t[..i]
                    .iter()
                    .rev()
                    .find_map(|p| out.iter().position(|o| o.0 == p.0))
                    .map(|p| p + 1)
                    .unwrap_or(if i == 0 { 0 } else { out.len() });
                out.insert(at, (k.clone(), tb.clone()));
            }
        }
    }
    Merged { text: join(&out.into_iter().map(|x| x.1).collect::<Vec<_>>()), taken, conflicts }
}

/// What changed from `old` to `new`, by section: "How to work on it, Roadmap (new), Design
/// (removed)", or "" when nothing did.
pub fn changed_sections(old: &str, new: &str) -> String {
    let (o, n) = (keyed(old), keyed(new));
    let mut out = Vec::new();
    for (k, title, body) in &n {
        match o.iter().find(|x| &x.0 == k) {
            Some(x) if same(Some(&x.2), Some(body)) => {}
            Some(_) => out.push(title.clone()),
            None => out.push(format!("{title} (new)")),
        }
    }
    for (k, title, _) in &o {
        if !n.iter().any(|x| &x.0 == k) {
            out.push(format!("{title} (removed)"));
        }
    }
    out.join(", ")
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

    #[test]
    fn edits_merge_by_section_and_conflicts_keep_the_owners_version() {
        let base = "Intro.\n\n## A\n\na\n\n## B\n\nb\n\n## C\n\nc\n";
        // The owner changed A and removed C; the agent changed B and added D after B.
        let mine = "Intro.\n\n## A\n\na, mine\n\n## B\n\nb\n";
        let theirs = "Intro.\n\n## A\n\na\n\n## B\n\nb, theirs\n\n## D\n\nd\n\n## C\n\nc\n";
        let m = merge(base, theirs, mine);
        assert_eq!(m.text, "Intro.\n\n## A\n\na, mine\n\n## B\n\nb, theirs\n\n## D\n\nd");
        assert_eq!(m.taken, ["B", "D"]);
        assert!(m.conflicts.is_empty());
        // Both changed B: the owner's stays, the agent's is offered.
        let mine = "Intro.\n\n## A\n\na\n\n## B\n\nb, mine\n\n## C\n\nc\n";
        let m = merge(base, theirs, mine);
        assert!(m.text.contains("b, mine") && !m.text.contains("b, theirs"));
        assert_eq!(
            m.conflicts,
            [Conflict { title: "B".into(), theirs: "## B\n\nb, theirs\n\n".into(), mine_kept: true }]
        );
        // The owner removed B, which the agent changed: offered back.
        let m = merge(base, theirs, "Intro.\n\n## A\n\na\n\n## C\n\nc\n");
        assert_eq!(m.conflicts.len(), 1);
        assert!(!m.conflicts[0].mine_kept && !m.text.contains("## B"));
        // Nothing changed on the agent's side: the owner's text as it is.
        assert_eq!(merge(base, base, mine).text, mine.trim_end());
        assert_eq!(changed_sections(base, mine), "B");
        assert_eq!(changed_sections(base, theirs), "B, D (new)");
        assert_eq!(changed_sections(base, "## A\n\na\n"), "Introduction (removed), B (removed), C (removed)");
    }
}
