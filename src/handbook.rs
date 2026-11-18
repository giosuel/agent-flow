//! The handbook: the owner's rules for every project (how they work, the machine, development and
//! procedures), once the "guides" in the ai-handbook git repository. The owner, 2026-11-18:
//! "Lets rename \"guides\" to \"handbook\" and I dont like the PR system there", then "lets
//! actually make it so its not in git, we can just have it as actual files, and then have the
//! backup make sure we don't lose anything. this way we don't have to care about the repo".
//!
//! Every file is kept in the database (so the daily backup holds it), each edit a revision; they're
//! written out as plain files under `<data_dir>/handbook`, and `flow` keeps a copy on every machine
//! (`~/.local/share/agentflow/handbook`), which Claude Code's `~/.claude/CLAUDE.md` includes. The
//! owner edits on the Handbook page, the consultant through MCP; agents read through MCP.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::util::now_ms;

/// One text file can't be bigger than this.
pub const MAX_FILE: usize = 512 * 1024;
/// A binary file (an image) can't be bigger than this.
pub const MAX_BIN: usize = 5 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Section {
    /// The heading line without its #s; "Introduction" for the text before the first `##`.
    pub title: String,
    pub level: usize,
    /// The section's text, heading line included.
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct File {
    /// e.g. "MACHINE.md"
    pub name: String,
    pub text: String,
    pub sections: Vec<Section>,
    pub rev: i64,
    pub updated_at: i64,
    pub author: String,
}

impl File {
    /// A short fingerprint of the content, so a save can tell the file changed meanwhile.
    pub fn version(&self) -> String {
        crate::util::hex(&Sha256::digest(self.text.as_bytes())[..8])
    }

    pub fn find_section(&self, title: &str) -> Option<usize> {
        let want = title.trim().trim_start_matches('#').trim().to_lowercase();
        self.sections.iter().position(|s| s.title.to_lowercase() == want)
    }
}

/// A path in the handbook: parts of letters, digits, `-`, `_` and `.`, none starting with a dot,
/// at most four deep ("MACHINE.md", "templates/external-watch/README.md").
pub fn check_path(path: &str) -> anyhow::Result<()> {
    let parts: Vec<&str> = path.split('/').collect();
    let ok = !path.is_empty()
        && path.len() <= 200
        && parts.len() <= 4
        && parts.iter().all(|p| {
            !p.is_empty() && !p.starts_with('.') && p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        });
    if !ok {
        bail!("{path:?} isn't a handbook file; they're paths like MACHINE.md or templates/x/README.md");
    }
    Ok(())
}

/// The handbook's pages: the top-level Markdown files (what the page and `handbook` list), README
/// last. The other files (templates, scripts) are kept and copied to machines too.
pub fn is_page(path: &str) -> bool {
    !path.contains('/') && path.ends_with(".md")
}

/// Every file's path, pages first in the page's order, then the rest.
pub fn paths(conn: &Connection) -> anyhow::Result<Vec<String>> {
    let mut st = conn.prepare("SELECT path FROM handbook_files")?;
    let mut all: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    all.sort_by_key(|p| (!is_page(p), p == "README.md", p.clone()));
    Ok(all)
}

pub fn pages(conn: &Connection) -> anyhow::Result<Vec<String>> {
    Ok(paths(conn)?.into_iter().filter(|p| is_page(p)).collect())
}

pub fn read(conn: &Connection, path: &str) -> anyhow::Result<File> {
    check_path(path)?;
    let binary: bool = conn
        .query_row("SELECT bin IS NOT NULL FROM handbook_files WHERE path = ?1", [path], |r| r.get(0))
        .optional()?
        .unwrap_or(false);
    if binary {
        bail!("{path} is a binary file (an image); it's in the copy on every machine");
    }
    conn.query_row("SELECT text, rev, updated_at, author FROM handbook_files WHERE path = ?1", [path], |r| {
        let text: String = r.get(0)?;
        Ok(File {
            name: path.to_owned(),
            sections: split(&text),
            text,
            rev: r.get(1)?,
            updated_at: r.get(2)?,
            author: r.get(3)?,
        })
    })
    .optional()?
    .with_context(|| format!("the handbook has no {path}"))
}

/// Splits at `##` headings (and deeper ones stay inside their `##` section). Code blocks are
/// respected, so a `## ` inside ``` isn't a heading.
pub fn split(text: &str) -> Vec<Section> {
    let mut sections = Vec::new();
    let mut current = Section { title: "Introduction".into(), level: 1, text: String::new() };
    let mut in_code = false;
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
        }
        if !in_code && line.starts_with("## ") {
            if !current.text.trim().is_empty() {
                sections.push(current);
            }
            current = Section { title: line[3..].trim().to_owned(), level: 2, text: String::new() };
        }
        current.text.push_str(line);
    }
    if !current.text.trim().is_empty() {
        sections.push(current);
    }
    sections
}

/// The file with one section replaced.
pub fn replace_section(g: &File, index: usize, new_text: &str) -> anyhow::Result<String> {
    if index >= g.sections.len() {
        bail!("no such section");
    }
    let mut new_text = new_text.replace("\r\n", "\n");
    if new_text.trim().is_empty() {
        bail!("a section can't be empty; to remove it, edit the whole file");
    }
    if !new_text.ends_with('\n') {
        new_text.push('\n');
    }
    // Keep the blank line that separated it from the next section.
    if index + 1 < g.sections.len() && !new_text.ends_with("\n\n") {
        new_text.push('\n');
    }
    let mut out = String::new();
    for (i, s) in g.sections.iter().enumerate() {
        out.push_str(if i == index { &new_text } else { &s.text });
    }
    Ok(out)
}

/// Why a save didn't happen; the page and the tool say each in their own words.
#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("the file changed since it was read; read it again and redo the edit")]
    Stale,
    #[error("nothing changed")]
    Unchanged,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<rusqlite::Error> for SaveError {
    fn from(e: rusqlite::Error) -> Self {
        SaveError::Other(e.into())
    }
}

/// Writes a file as a new revision: a new file when `version` is empty and there's none; else only
/// over the version the editor read. An empty text removes the file. Returns the new revision.
pub fn save(
    conn: &Connection,
    path: &str,
    version: &str,
    text: &str,
    author: &str,
    what: &str,
) -> Result<i64, SaveError> {
    check_path(path)?;
    let text = text.replace("\r\n", "\n");
    if text.len() > MAX_FILE {
        return Err(anyhow::anyhow!("a handbook file can be {} KB at most", MAX_FILE / 1024).into());
    }
    let cur = read(conn, path).ok();
    match &cur {
        Some(c) if c.version() != version => return Err(SaveError::Stale),
        Some(c) if c.text == text => return Err(SaveError::Unchanged),
        None if text.trim().is_empty() => return Err(SaveError::Unchanged),
        _ => {}
    }
    let rev = cur.as_ref().map(|c| c.rev).unwrap_or(0) + 1;
    let now = now_ms();
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO handbook_revs (path, rev, at, author, what, text) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![path, rev, now, author, what, text],
    )?;
    if text.trim().is_empty() {
        tx.execute("DELETE FROM handbook_files WHERE path = ?1", [path])?;
    } else {
        tx.execute(
            "INSERT INTO handbook_files (path, text, rev, updated_at, author, what) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (path) DO UPDATE SET text = ?2, bin = NULL, rev = ?3, updated_at = ?4, author = ?5, what = ?6",
            params![path, text, rev, now, author, what],
        )?;
    }
    tx.commit()?;
    Ok(rev)
}

#[derive(Debug, Clone, Serialize)]
pub struct Rev {
    pub rev: i64,
    pub at: i64,
    pub author: String,
    pub what: String,
}

pub fn revisions(conn: &Connection, path: &str, limit: i64) -> anyhow::Result<Vec<Rev>> {
    let mut st =
        conn.prepare("SELECT rev, at, author, what FROM handbook_revs WHERE path = ?1 ORDER BY rev DESC LIMIT ?2")?;
    let rows = st.query_map(params![path, limit], |r| {
        Ok(Rev { rev: r.get(0)?, at: r.get(1)?, author: r.get(2)?, what: r.get(3)? })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn revision_text(conn: &Connection, path: &str, rev: i64) -> anyhow::Result<Option<String>> {
    Ok(conn
        .query_row("SELECT text FROM handbook_revs WHERE path = ?1 AND rev = ?2", params![path, rev], |r| r.get(0))
        .optional()?)
}

/// Writes a binary file (an image) as a new revision, over whatever is there.
pub fn save_bin(conn: &Connection, path: &str, bytes: &[u8], author: &str, what: &str) -> anyhow::Result<i64> {
    check_path(path)?;
    if bytes.len() > MAX_BIN {
        bail!("a binary handbook file can be {} MB at most", MAX_BIN / 1024 / 1024);
    }
    let rev: i64 =
        conn.query_row("SELECT rev FROM handbook_files WHERE path = ?1", [path], |r| r.get(0)).optional()?.unwrap_or(0)
            + 1;
    let now = now_ms();
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO handbook_revs (path, rev, at, author, what, text, bin) VALUES (?1, ?2, ?3, ?4, ?5, '', ?6)",
        params![path, rev, now, author, what, bytes],
    )?;
    tx.execute(
        "INSERT INTO handbook_files (path, text, bin, rev, updated_at, author, what) VALUES (?1, '', ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (path) DO UPDATE SET text = '', bin = ?2, rev = ?3, updated_at = ?4, author = ?5, what = ?6",
        params![path, bytes, rev, now, author, what],
    )?;
    tx.commit()?;
    Ok(rev)
}

/// Files as (path, bytes).
pub type Files = Vec<(String, Vec<u8>)>;

/// Every file's bytes and a fingerprint of them all, for the copies on machines.
pub fn bundle(conn: &Connection) -> anyhow::Result<(String, Files)> {
    let mut st = conn.prepare("SELECT path, text, bin FROM handbook_files ORDER BY path")?;
    let files: Vec<(String, Vec<u8>)> = st
        .query_map([], |r| {
            let bin: Option<Vec<u8>> = r.get(2)?;
            Ok((r.get(0)?, bin.unwrap_or(r.get::<_, String>(1)?.into_bytes())))
        })?
        .collect::<Result<_, _>>()?;
    let mut h = Sha256::new();
    for (p, t) in &files {
        h.update(p.as_bytes());
        h.update([0]);
        h.update(t);
        h.update([0]);
    }
    Ok((crate::util::hex(&h.finalize()[..8]), files))
}

/// Writes the files into `dir` as they are: new and changed files replaced whole (a temporary file,
/// then a rename), files no longer in the handbook removed, `.version` noting which version it is.
/// Only files the handbook names are touched.
pub fn write_out(dir: &Path, version: &str, files: &[(String, Vec<u8>)]) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
    let listed = dir.join(".files");
    let before: Vec<String> = std::fs::read_to_string(&listed).unwrap_or_default().lines().map(str::to_owned).collect();
    for (path, text) in files {
        check_path(path)?;
        let to = dir.join(path);
        if std::fs::read(&to).ok().as_deref() == Some(text.as_slice()) {
            continue;
        }
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = to.with_extension("agentflow-tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &to)?;
    }
    for old in before.iter().filter(|o| !files.iter().any(|(p, _)| p == *o)) {
        if check_path(old).is_ok() {
            let _ = std::fs::remove_file(dir.join(old));
        }
    }
    std::fs::write(&listed, files.iter().map(|(p, _)| format!("{p}\n")).collect::<String>())?;
    std::fs::write(dir.join(".version"), version)?;
    Ok(())
}

/// The folder the server keeps the files in.
pub fn server_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("handbook")
}

/// Writes the server's own copy of the files.
pub fn mirror(conn: &Connection, data_dir: &Path) -> anyhow::Result<()> {
    let (version, files) = bundle(conn)?;
    write_out(&server_dir(data_dir), &version, &files)
}

/// Brings the files in once, from the old ai-handbook checkout, when the handbook is still empty:
/// every text file of the tree, `.git` and dot files left out. Returns how many came in.
pub fn import(conn: &Connection, from: &Path) -> anyhow::Result<usize> {
    let have: i64 = conn.query_row("SELECT count(*) FROM handbook_revs", [], |r| r.get(0))?;
    if have > 0 || !from.is_dir() {
        return Ok(0);
    }
    let mut found = Vec::new();
    walk(from, from, &mut found)?;
    let mut n = 0;
    let what = "brought in from the ai-handbook repository";
    for (path, bytes) in found {
        let done = match String::from_utf8(bytes) {
            Ok(text) => save(conn, &path, "", &text, "import", what).is_ok(),
            Err(e) => save_bin(conn, &path, e.as_bytes(), "import", what).is_ok(),
        };
        if done {
            n += 1;
        } else {
            tracing::warn!("handbook import: {path} left out (too big?)");
        }
    }
    Ok(n)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> anyhow::Result<()> {
    for e in std::fs::read_dir(dir)?.flatten() {
        let p = e.path();
        let Ok(rel) = p.strip_prefix(root) else { continue };
        let rel = rel.to_string_lossy().to_string();
        if e.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let ft = e.file_type()?;
        if ft.is_dir() {
            walk(root, &p, out)?;
        } else if ft.is_file() && check_path(&rel).is_ok() {
            out.push((rel, std::fs::read(&p)?));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "# MACHINE\n\nIntro.\n\n## The machine\n\n- 8 cores\n\n```sh\n## not a heading\n```\n\n## Ports\n\nUse 10.0.8.1.\n";

    #[test]
    fn sections_split_and_replace() -> anyhow::Result<()> {
        let s = split(TEXT);
        assert_eq!(s.iter().map(|x| x.title.as_str()).collect::<Vec<_>>(), ["Introduction", "The machine", "Ports"]);
        assert!(s[1].text.contains("## not a heading"), "code blocks stay whole");
        let g = File {
            name: "MACHINE.md".into(),
            text: TEXT.into(),
            sections: s,
            rev: 1,
            updated_at: 0,
            author: String::new(),
        };
        assert_eq!(g.find_section("## ports"), Some(2));
        let out = replace_section(&g, 1, "## The machine\n\n- 8 vCPU, 22 GiB")?;
        assert!(out.contains("- 8 vCPU, 22 GiB\n\n## Ports"));
        assert!(replace_section(&g, 1, "  ").is_err());
        assert_eq!(split(&out).len(), 3);
        Ok(())
    }

    #[test]
    fn paths() {
        for ok in ["MACHINE.md", "templates/external-watch/README.md", "templates/x.sh"] {
            assert!(check_path(ok).is_ok(), "{ok}");
        }
        for bad in ["../x.md", "/x.md", ".hidden.md", "a//b.md", "a/.git/config", "x.md\n", "a/b/c/d/e.md"] {
            assert!(check_path(bad).is_err(), "{bad:?}");
        }
        assert!(is_page("OWNER.md") && !is_page("templates/x.md") && !is_page("x.sh"));
    }

    #[test]
    fn saves_are_revisions_and_refuse_stale_edits_and_files_are_written_out() {
        let db = crate::db::Db::open_memory();
        db.call_blocking(|c| {
            // Imported once from a checkout: text files, dot files and .git left out.
            let src = tempfile::tempdir()?;
            std::fs::write(src.path().join("MACHINE.md"), TEXT)?;
            std::fs::create_dir_all(src.path().join("templates/w"))?;
            std::fs::write(src.path().join("templates/w/run.sh"), "echo hi\n")?;
            std::fs::create_dir_all(src.path().join(".git"))?;
            std::fs::write(src.path().join(".git/config"), "x")?;
            std::fs::write(src.path().join(".env"), "x")?;
            std::fs::write(src.path().join("logo.png"), [0x89, b'P', b'N', b'G', 0xff, 0x00])?;
            assert_eq!(import(c, src.path())?, 3);
            assert!(read(c, "logo.png").is_err(), "binary files aren't pages");
            assert_eq!(import(c, src.path())?, 0, "only once");
            assert_eq!(pages(c)?, ["MACHINE.md"]);
            let g = read(c, "MACHINE.md")?;
            let new = replace_section(&g, 2, "## Ports\n\nUse 10.0.8.1 or 127.0.0.1.")?;
            assert_eq!(save(c, "MACHINE.md", &g.version(), &new, "owner", "Ports").unwrap(), 2);
            assert!(matches!(save(c, "MACHINE.md", &g.version(), TEXT, "owner", "x"), Err(SaveError::Stale)));
            let now = read(c, "MACHINE.md")?;
            assert!(matches!(save(c, "MACHINE.md", &now.version(), &new, "owner", "x"), Err(SaveError::Unchanged)));
            assert_eq!(revisions(c, "MACHINE.md", 10)?.len(), 2);
            assert_eq!(revision_text(c, "MACHINE.md", 1)?.as_deref(), Some(TEXT));
            // Written out; a removed file goes from the folder too.
            let out = tempfile::tempdir()?;
            let (v, files) = bundle(c)?;
            write_out(out.path(), &v, &files)?;
            assert!(std::fs::read_to_string(out.path().join("MACHINE.md"))?.contains("127.0.0.1"));
            assert!(out.path().join("templates/w/run.sh").exists());
            assert_eq!(std::fs::read(out.path().join("logo.png"))?, [0x89, b'P', b'N', b'G', 0xff, 0x00]);
            std::fs::write(out.path().join("mine.txt"), "kept")?;
            save(c, "templates/w/run.sh", &read(c, "templates/w/run.sh")?.version(), "", "consultant", "removed")
                .unwrap();
            let (v2, files) = bundle(c)?;
            assert_ne!(v, v2);
            write_out(out.path(), &v2, &files)?;
            assert!(!out.path().join("templates/w/run.sh").exists() && out.path().join("mine.txt").exists());
            assert_eq!(std::fs::read_to_string(out.path().join(".version"))?, v2);
            Ok(())
        })
        .unwrap();
    }
}
