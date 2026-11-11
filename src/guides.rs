//! Guides: the owner's rules for every project, kept as Markdown files in the ai-handbook git
//! repository (the owner, 2026-11-07: global content "keeps being part of a separate repo, but the
//! agents reach it through agent flow and they can be edited in the agent flow interface").
//!
//! Agents read them through MCP. The owner edits a file or a section on the page, which commits to
//! the checkout's main branch as the owner and never pushes (the owner, 2026-11-11: "yes commit
//! directly, but don't push"). The consultant still changes them with pull requests.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Section {
    /// The heading line without its #s; "Introduction" for the text before the first `##`.
    pub title: String,
    pub level: usize,
    /// The section's text, heading line included.
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Guide {
    /// e.g. "MACHINE.md"
    pub name: String,
    pub text: String,
    pub sections: Vec<Section>,
}

impl Guide {
    /// A short fingerprint of the content, so a save can tell the file changed meanwhile.
    pub fn version(&self) -> String {
        crate::util::hex(&Sha256::digest(self.text.as_bytes())[..8])
    }

    pub fn find_section(&self, title: &str) -> Option<usize> {
        let want = title.trim().trim_start_matches('#').trim().to_lowercase();
        self.sections.iter().position(|s| s.title.to_lowercase() == want)
    }
}

/// Guide names are plain Markdown file names at the repository's top level.
pub fn check_name(name: &str) -> anyhow::Result<()> {
    let ok = name.ends_with(".md")
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if !ok {
        bail!("{name:?} isn't a guide; guides are .md files like MACHINE.md");
    }
    Ok(())
}

/// The guides: every top-level `.md` file, README last. Read as the agents' user (see
/// [`crate::hands`]), like everything in the checkout.
pub fn list(dir: &Path) -> anyhow::Result<Vec<String>> {
    crate::hands::run(crate::hands::Job::GuideList { dir: dir.to_owned() })
}

pub fn read(dir: &Path, name: &str) -> anyhow::Result<Guide> {
    check_name(name)?;
    crate::hands::run(crate::hands::Job::GuideRead { dir: dir.to_owned(), name: name.to_owned() })
}

pub(crate) fn list_here(dir: &Path) -> anyhow::Result<Vec<String>> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| check_name(n).is_ok())
        .collect();
    names.sort_by_key(|n| (n == "README.md", n.clone()));
    Ok(names)
}

pub(crate) fn read_here(dir: &Path, name: &str) -> anyhow::Result<Guide> {
    check_name(name)?;
    // A link could point anywhere on the machine; guides are plain files in the checkout.
    let meta = std::fs::symlink_metadata(dir.join(name)).with_context(|| format!("no guide {name}"))?;
    if !meta.file_type().is_file() {
        bail!("{name} isn't a plain file");
    }
    let text = std::fs::read_to_string(dir.join(name)).with_context(|| format!("no guide {name}"))?;
    Ok(Guide { name: name.to_owned(), sections: split(&text), text })
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
pub fn replace_section(g: &Guide, index: usize, new_text: &str) -> anyhow::Result<String> {
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

/// git in the handbook checkout, with its hooks and fsmonitor off: whoever can write to the
/// checkout's .git/config shouldn't get to run code as AgentFlow.
pub fn git_command(dir: &Path) -> std::process::Command {
    let argv = git_argv(dir);
    let mut c = std::process::Command::new(&argv[0]);
    c.args(&argv[1..]);
    c
}

/// The same as a command line, for a job.
pub fn git_argv(dir: &Path) -> Vec<String> {
    ["git", "-C", &dir.to_string_lossy(), "-c", "core.hooksPath=/dev/null", "-c", "core.fsmonitor=false"]
        .map(String::from)
        .to_vec()
}

/// Why a save didn't happen; the page says each in its own words.
#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("the file changed since it was opened")]
    Stale,
    #[error("the handbook checkout isn't on main")]
    NotOnMain,
    #[error("the file has uncommitted changes in the checkout")]
    Dirty,
    #[error("nothing changed")]
    Unchanged,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl SaveError {
    /// Its name across the agents' helper's socket.
    pub fn code(&self) -> &'static str {
        match self {
            SaveError::Stale => "stale",
            SaveError::NotOnMain => "not_on_main",
            SaveError::Dirty => "dirty",
            SaveError::Unchanged => "unchanged",
            SaveError::Other(_) => "other",
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        Some(match code {
            "stale" => SaveError::Stale,
            "not_on_main" => SaveError::NotOnMain,
            "dirty" => SaveError::Dirty,
            "unchanged" => SaveError::Unchanged,
            _ => return None,
        })
    }
}

impl From<std::io::Error> for SaveError {
    fn from(e: std::io::Error) -> Self {
        SaveError::Other(e.into())
    }
}

/// Where the owner's edits are committed, and as whom.
pub struct Repo {
    pub dir: PathBuf,
    pub author: String,
}

impl Repo {
    fn git(&self, args: &[&str]) -> anyhow::Result<String> {
        let out = git_command(&self.dir).args(args).output().context("running git")?;
        if !out.status.success() {
            bail!("git {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// Writes `content` to a guide and commits it on main as the owner, as the agents' user.
    /// Refuses when the file changed since the page showed it (`version`), when the checkout isn't
    /// on main, or when the file has uncommitted changes of someone else's.
    pub fn save(&self, name: &str, version: &str, content: &str, what: &str) -> Result<String, SaveError> {
        let job = crate::hands::Job::GuideSave {
            dir: self.dir.clone(),
            author: self.author.clone(),
            name: name.to_owned(),
            version: version.to_owned(),
            content: content.to_owned(),
            what: what.to_owned(),
        };
        crate::hands::run(job).map_err(|e| {
            match e.downcast_ref::<crate::hands::Coded>().and_then(|c| SaveError::from_code(&c.code)) {
                Some(known) => known,
                None => SaveError::Other(e),
            }
        })
    }

    pub(crate) fn save_here(&self, name: &str, version: &str, content: &str, what: &str) -> Result<String, SaveError> {
        check_name(name)?;
        let current = read_here(&self.dir, name)?;
        if current.version() != version {
            return Err(SaveError::Stale);
        }
        if self.git(&["rev-parse", "--abbrev-ref", "HEAD"])? != "main" {
            return Err(SaveError::NotOnMain);
        }
        if !self.git(&["status", "--porcelain", "--", name])?.is_empty() {
            return Err(SaveError::Dirty);
        }
        if content == current.text {
            return Err(SaveError::Unchanged);
        }
        std::fs::write(self.dir.join(name), content)?;
        let (author_name, email) =
            self.author.split_once(" <").map(|(n, e)| (n, e.trim_end_matches('>'))).unwrap_or((&self.author, ""));
        let subject = format!("Edit {name}: {what}");
        let result = self.git(&[
            "-c",
            &format!("user.name={author_name}"),
            "-c",
            &format!("user.email={email}"),
            "commit",
            "-q",
            "--no-verify",
            "-m",
            &subject,
            "-m",
            "Edited in AgentFlow by the owner. Not pushed: push it from the checkout when ready.",
            "--",
            name,
        ]);
        if let Err(e) = result {
            // Leave the checkout as it was.
            let _ = self.git(&["checkout", "--", name]);
            return Err(e.into());
        }
        Ok(self.git(&["rev-parse", "--short", "HEAD"])?)
    }

    /// Commits on main that aren't on origin/main yet (the owner's own edits, waiting for a push).
    pub fn unpushed(&self) -> usize {
        crate::hands::run(crate::hands::Job::GuideUnpushed { dir: self.dir.clone() }).unwrap_or(0)
    }

    pub(crate) fn unpushed_here(&self) -> usize {
        self.git(&["rev-list", "--count", "origin/main..main"]).ok().and_then(|n| n.parse().ok()).unwrap_or(0)
    }
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
        let g = Guide { name: "MACHINE.md".into(), text: TEXT.into(), sections: s };
        assert_eq!(g.find_section("## ports"), Some(2));
        let out = replace_section(&g, 1, "## The machine\n\n- 8 vCPU, 22 GiB")?;
        assert!(out.contains("- 8 vCPU, 22 GiB\n\n## Ports"));
        assert!(replace_section(&g, 1, "  ").is_err());
        assert_eq!(split(&out).len(), 3);
        Ok(())
    }

    #[test]
    fn names() {
        assert!(check_name("MACHINE.md").is_ok());
        for bad in ["../x.md", "a/b.md", ".hidden.md", "x.sh", "x.md\n"] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/etc/hostname", dir.path().join("HOST.md")).unwrap();
        assert!(read_here(dir.path(), "HOST.md").is_err(), "links aren't followed");
    }

    #[test]
    fn saves_commit_locally_and_refuse_stale_edits() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git").arg("-C").arg(d).args(args).output().unwrap();
            assert!(ok.status.success(), "{:?}", String::from_utf8_lossy(&ok.stderr));
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(d.join("MACHINE.md"), TEXT).unwrap();
        git(&["add", "MACHINE.md"]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "init"]);
        let repo = Repo { dir: d.to_owned(), author: "giosuel <me@giosuel.ch>".into() };
        let g = read(d, "MACHINE.md").unwrap();
        let new = replace_section(&g, 2, "## Ports\n\nUse 10.0.8.1 or 127.0.0.1.").unwrap();
        repo.save("MACHINE.md", &g.version(), &new, "Ports").unwrap();
        let log = std::process::Command::new("git")
            .arg("-C")
            .arg(d)
            .args(["log", "-1", "--format=%an <%ae>|%s"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "giosuel <me@giosuel.ch>|Edit MACHINE.md: Ports");
        // The old version is stale now.
        assert!(matches!(repo.save("MACHINE.md", &g.version(), TEXT, "x"), Err(SaveError::Stale)));
        // Someone's uncommitted change is never swept into a commit.
        std::fs::write(d.join("MACHINE.md"), "dirty\n").unwrap();
        let g2 = read(d, "MACHINE.md").unwrap();
        assert!(matches!(repo.save("MACHINE.md", &g2.version(), "mine\n", "x"), Err(SaveError::Dirty)));
    }
}
