//! Notes: any file or a short text an agent hands in, with a name and a category (the owner,
//! 2026-11-12: "some generic upload "note" endpoint so agents can submit any kind of files, like
//! morning reports, estimates, etc."; technical notes and known issues "fit together with the notes").
//! Each is a record (N-4): title = name, body = the text of a text note; data = category, the file,
//! and when it should come back to the agent (a date, or a condition such as "when Loro updates").
//! Files are kept in `<data_dir>/notes/<project>/N-4.<ext>`, older versions beside it; they're only
//! ever downloaded, never shown as a page, except Markdown and plain text, which the page renders.

use std::path::{Path, PathBuf};

use anyhow::bail;

/// The largest file accepted.
pub const MAX_BYTES: usize = 25 * 1024 * 1024;

/// A file's extension, from its name: lowercase letters and digits, up to 8; "bin" otherwise.
pub fn ext_of(name: &str) -> String {
    let e = Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if !e.is_empty() && e.len() <= 8 && e.chars().all(|c| c.is_ascii_alphanumeric()) { e } else { "bin".into() }
}

/// Text the page may show: Markdown and plain text (not HTML or SVG: never served from our origin).
pub fn readable(ext: &str) -> bool {
    matches!(ext, "md" | "markdown" | "txt" | "log" | "csv" | "tsv" | "json" | "toml" | "yaml" | "yml")
}

pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("notes")
}

/// Where a note's file is kept (`version` > 0 for an older one). Slugs, numbers and extensions
/// are checked values, so the path stays inside the folder.
pub fn path(data_dir: &Path, slug: &str, number: i64, ext: &str, version: i64) -> PathBuf {
    let name = if version > 0 { format!("N-{number}.v{version}.{ext}") } else { format!("N-{number}.{ext}") };
    dir(data_dir).join(slug).join(name)
}

/// When a note comes back to the agent: a date ("2026-11-25") or a condition ("when Loro
/// updates"), as written. Returns (date in ms if it's one, the text).
pub fn resurface(raw: &str) -> anyhow::Result<(Option<i64>, String)> {
    let r = raw.trim();
    if r.chars().count() > 120 {
        bail!("say when it comes back in a few words, or as a date (2026-11-25)");
    }
    let f = time::macros::format_description!("[year]-[month]-[day]");
    Ok(match time::Date::parse(r, &f) {
        Ok(d) => (Some((d.midnight().assume_utc().unix_timestamp_nanos() / 1_000_000) as i64), r.to_owned()),
        Err(_) => (None, r.to_owned()),
    })
}

/// What a note belongs to: a record of the project ("T-12") or a step or milestone ("P9.R").
pub fn target(
    c: &rusqlite::Connection,
    project: &crate::projects::Project,
    what: &str,
) -> anyhow::Result<(Option<crate::records::Record>, Option<i64>)> {
    if what.is_empty() {
        return Ok((None, None));
    }
    if let Ok(r) = crate::records::RecordRef::parse(what)
        && let Some(rec) = crate::records::try_find(c, &r, Some(&project.slug))?
    {
        if rec.project_id != project.id {
            bail!("{} belongs to another project", rec.ref_id);
        }
        return Ok((Some(rec), None));
    }
    match crate::projects::find_milestone(c, project.id, what) {
        Ok(m) => Ok((None, Some(m.id))),
        Err(_) => bail!("{} has no record or step {what:?}", project.slug),
    }
}

/// Size as people read it.
pub fn size(bytes: i64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / 1048576.0)
    } else if bytes >= 1024 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_resurfacing() {
        assert_eq!(ext_of("morning-report.md"), "md");
        assert_eq!(ext_of("Report.PDF"), "pdf");
        assert_eq!(ext_of("noext"), "bin");
        assert_eq!(ext_of("x.tar.gz"), "gz");
        assert_eq!(ext_of("evil.html "), "bin");
        assert!(readable("md") && !readable("html") && !readable("svg"));
        assert!(resurface("2026-11-25").unwrap().0.is_some());
        assert_eq!(resurface(" when Loro updates ").unwrap(), (None, "when Loro updates".into()));
        assert_eq!(size(2048), "2 KB");
    }
}
