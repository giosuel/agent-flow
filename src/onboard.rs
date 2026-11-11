//! Starting a project from the New project button: the record and an M0 milestone, its folder with
//! a local git repository, a project token in its file, and the agent's first note in its memory.
//! The owner, 2026-11-07: "i can create the repo, and then later on tell the agent to use the
//! remote", so nothing is created on GitHub. Registering AgentFlow with Claude Code is the
//! owner's command to run (it changes Claude's own settings), so it's shown, not run.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use rusqlite::Connection;

use crate::auth::{self, TokenKind};
use crate::projects::{self, NewMilestone, ProjectPatch};
use crate::util::expand_home;

#[derive(Debug, Clone, Default)]
pub struct NewProject {
    pub name: String,
    pub slug: String,
    /// e.g. "~/speculum"
    pub folder: String,
    pub description: String,
    pub stack: Vec<String>,
    pub platform: String,
    pub model: String,
    pub effort: String,
}

/// What was done, step by step, and the commands left for the owner.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub slug: String,
    pub steps: Vec<String>,
    pub mcp_add: String,
    pub start: String,
}

/// "Speculum Tool" → "speculum-tool"
pub fn slug_for(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').chars().take(40).collect()
}

/// The folder must be under the home directory, and either new or an existing folder (onboarding).
fn check_folder(folder: &str) -> anyhow::Result<PathBuf> {
    let folder = folder.trim();
    let path = if folder == "~" { crate::util::home_dir() } else { expand_home(folder) };
    let home = crate::util::home_dir();
    if !path.is_absolute() || !path.starts_with(&home) || path == home {
        bail!("the folder must be inside your home folder, e.g. ~/speculum");
    }
    if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        bail!("the folder can't contain ..");
    }
    // It goes into shell commands shown to the owner; keep it plain so they work as copied.
    if !path.to_string_lossy().chars().all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c)) {
        bail!("use letters, digits, dots, dashes and underscores in the folder's name");
    }
    // No link anywhere on the way: the project would work in wherever it points.
    let mut walk = home.clone();
    for part in path.strip_prefix(&home).unwrap_or(&path).components() {
        walk.push(part);
        match std::fs::symlink_metadata(&walk) {
            Ok(m) if m.file_type().is_symlink() => bail!("{} is a link; give the real folder", walk.display()),
            Ok(m) if !m.is_dir() => bail!("{} exists and isn't a folder", walk.display()),
            Ok(_) => {}
            Err(_) => break, // the rest doesn't exist yet
        }
    }
    Ok(path)
}

/// Folders a new project may not use: other projects' checkouts and AgentFlow's and Claude
/// Code's own, with everything inside them (and their parents, which would contain them).
pub struct Taken {
    pub dirs: Vec<(PathBuf, String)>,
    pub token_dir: PathBuf,
}

impl Taken {
    pub fn load(conn: &Connection, config: &crate::config::Config) -> anyhow::Result<Self> {
        let mut dirs: Vec<(PathBuf, String)> = projects::list(conn)?
            .into_iter()
            .filter(|p| !p.checkout.trim().is_empty())
            .map(|p| (expand_home(p.checkout.trim()), format!("{}'s checkout", p.name)))
            .collect();
        for (dir, what) in [
            (config.token_dir(), "AgentFlow's tokens"),
            (config.data_dir(), "AgentFlow's data"),
            (config.handbook_dir(), "the guides"),
            (crate::util::home_dir().join(".claude"), "Claude Code's own folder"),
            (crate::util::home_dir().join(".config"), "your settings"),
        ] {
            dirs.push((dir, what.into()));
        }
        Ok(Self { dirs, token_dir: config.token_dir() })
    }

    fn check(&self, folder: &Path, slug: &str, name: &str) -> anyhow::Result<()> {
        for (dir, what) in &self.dirs {
            if folder.starts_with(dir) || dir.starts_with(folder) {
                bail!("{} overlaps {what} ({}); pick another folder", folder.display(), dir.display());
            }
        }
        let token = crate::tokens::token_path(&self.token_dir, Some(slug), name);
        if token.exists() {
            bail!(
                "{} exists already (another project's or the consultant's token); pick another name",
                token.display()
            );
        }
        Ok(())
    }
}

/// The database part: project, details, M0, token. Returns the token's secret.
pub fn create_records(c: &mut Connection, p: &NewProject, folder: &Path) -> anyhow::Result<(i64, String)> {
    let tx = c.transaction()?;
    let project = projects::create(&tx, &p.slug, p.name.trim(), "owner")?;
    let patch = ProjectPatch {
        description: Some(p.description.clone()),
        checkout: Some(folder.display().to_string()),
        machine: Some(crate::machine::hostname()),
        stack: Some(p.stack.clone()),
        platform: Some(p.platform.clone()),
        model: Some(p.model.clone()),
        effort: Some(p.effort.clone()),
        ..Default::default()
    };
    projects::update(&tx, project.id, &patch, "owner")?;
    projects::add_milestone(
        &tx,
        project.id,
        &NewMilestone { name: "M0", title: "Getting started", description: "", parent: None, before: None },
        "owner",
    )?;
    let (_, secret) = auth::create_token(&tx, TokenKind::Project, p.name.trim(), Some(&p.slug))?;
    tx.commit()?;
    Ok((project.id, secret))
}

/// The files: the folder and its git repository (an existing one is left as it is), the token
/// file, and the agent's first note. Each step says what it did.
pub fn create_files(
    p: &NewProject,
    folder: &Path,
    secret: &str,
    token_dir: &Path,
    claude_projects: &Path,
    public_url: &str,
) -> anyhow::Result<Outcome> {
    let mut steps = Vec::new();
    if folder.is_dir() {
        steps.push(format!("{} exists; left as it is.", folder.display()));
    } else {
        std::fs::create_dir_all(folder).with_context(|| format!("creating {}", folder.display()))?;
        steps.push(format!("Created {}.", folder.display()));
    }
    if folder.join(".git").exists() {
        steps.push("It's a git repository already.".into());
    } else {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(folder)
            .args(["-c", "core.hooksPath=/dev/null", "init", "-q", "-b", "main"])
            .output()
            .context("running git init")?;
        if !out.status.success() {
            bail!("git init: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        steps.push("Made it a git repository on main (no remote: create the GitHub repo and tell the agent).".into());
    }
    let token_file = crate::tokens::token_path(token_dir, Some(&p.slug), &p.name);
    crate::tokens::write_token_file(&token_file, secret)?;
    steps.push(format!("Wrote the project's token to {} (only you can read it).", token_file.display()));

    // The agent's memory lives with Claude Code, not in the repository (the owner, 2026-10-20).
    let memory = claude_projects.join(crate::usage::folder_for(&folder.display().to_string())).join("memory");
    if memory.exists() {
        steps.push(format!(
            "The agent's memory folder ({}) existed already; look through it before you start the agent, since it reads it.",
            memory.display()
        ));
    }
    std::fs::create_dir_all(&memory)?;
    let note = memory.join("agentflow.md");
    // create_new: a note somebody left there first is never taken as ours.
    let created = std::fs::OpenOptions::new().write(true).create_new(true).open(&note);
    if let Ok(mut f) = created {
        use std::io::Write;
        f.write_all(
            format!(
                "---\nname: agentflow\ndescription: this project ({name}) is run through AgentFlow; start every session with its hello tool\nmetadata:\n  type: project\n---\n\n\
                 {name} is managed in AgentFlow ({public_url}), through the `agentflow` MCP server. At the start of every \
                 session call `hello`, then `since_last_check`; record tasks, decisions and questions there, not in files \
                 in the repository. The owner's rules are in the guides (`guides`, `guide`). Ask the owner with \
                 `ask_question`; write to other agents and the consultant with `send_message`.\n",
                name = p.name.trim()
            )
            .as_bytes(),
        )?;
        let index = memory.join("MEMORY.md");
        let mut text = std::fs::read_to_string(&index).unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("- [AgentFlow](agentflow.md) — start each session with hello; records live in AgentFlow\n");
        std::fs::write(&index, text)?;
        steps.push(format!("Left the agent a first note in its memory ({}).", note.display()));
    }
    let bin = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "agentflow".into());
    let mcp_add = format!(
        "cd {} && claude mcp add-json agentflow '{{\"type\": \"http\", \"url\": \"{}/mcp\", \"headersHelper\": \"{bin} mcp-headers --token-file {}\"}}'",
        folder.display(),
        public_url.trim_end_matches('/'),
        token_file.display()
    );
    Ok(Outcome { slug: p.slug.clone(), steps, mcp_add, start: format!("cd {} && claude", folder.display()) })
}

/// Checks the form before anything is created.
pub fn check(p: &mut NewProject, taken: &Taken) -> anyhow::Result<PathBuf> {
    if p.name.trim().is_empty() || p.name.chars().count() > 60 {
        bail!("give the project a name (up to 60 characters)");
    }
    if p.slug.trim().is_empty() {
        p.slug = slug_for(&p.name);
    }
    projects::check_slug(&p.slug)?;
    if p.folder.trim().is_empty() {
        p.folder = format!("~/{}", p.slug);
    }
    let folder = check_folder(&p.folder)?;
    taken.check(&folder, &p.slug, &p.name)?;
    Ok(folder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_starts_with_its_folder_token_and_note() {
        assert_eq!(slug_for("Speculum Tool!"), "speculum-tool");
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("speculum");
        let p = NewProject { name: "Speculum".into(), slug: "speculum".into(), ..Default::default() };
        let db = crate::db::Db::open_memory();
        let f = folder.clone();
        let (id, secret) = db.call_blocking(move |c| create_records(c, &p, &f)).unwrap();
        let p = NewProject { name: "Speculum".into(), slug: "speculum".into(), ..Default::default() };
        let out = create_files(
            &p,
            &folder,
            &secret,
            &home.path().join("tokens"),
            &home.path().join("claude"),
            "https://flow.example",
        )
        .unwrap();
        assert!(folder.join(".git").exists());
        assert_eq!(std::fs::read_to_string(home.path().join("tokens/speculum.token")).unwrap().trim(), secret);
        assert!(out.mcp_add.contains("https://flow.example/mcp") && out.mcp_add.contains("tokens/speculum.token"));
        assert!(!out.mcp_add.contains(&secret), "the command never holds the token");
        let note = home
            .path()
            .join("claude")
            .join(crate::usage::folder_for(&folder.display().to_string()))
            .join("memory/agentflow.md");
        assert!(std::fs::read_to_string(note).unwrap().contains("hello"));
        db.call_blocking(move |c| {
            assert_eq!(projects::milestones(c, id)?[0].name, "M0");
            assert!(projects::get(c, "speculum")?.unwrap().checkout.ends_with("speculum"));
            Ok(())
        })
        .unwrap();
        // Onboarding an existing repository leaves it alone.
        let again =
            create_files(&p, &folder, &secret, &home.path().join("tokens"), &home.path().join("claude"), "x").unwrap();
        assert!(again.steps.iter().any(|s| s.contains("git repository already")));
    }

    #[test]
    fn folders_stay_in_home() {
        let tokens = tempfile::tempdir().unwrap();
        let none = Taken { dirs: Vec::new(), token_dir: tokens.path().to_owned() };
        for bad in ["/etc/x", "~", "~/../x", "~/my app", "~/it's"] {
            let mut p = NewProject { name: "X".into(), folder: bad.into(), ..Default::default() };
            assert!(check(&mut p, &none).is_err(), "{bad}");
        }
        let mut p = NewProject { name: "Cold Trace".into(), ..Default::default() };
        let path = check(&mut p, &none).unwrap();
        assert_eq!(p.slug, "cold-trace");
        assert!(path.ends_with("cold-trace"));
        // Another project's checkout, or a folder holding one, is taken; so is a token file.
        let home = crate::util::home_dir();
        let taken = Taken {
            dirs: vec![(home.join("vinculum"), "Vinculum's checkout".into())],
            token_dir: tokens.path().to_owned(),
        };
        for folder in ["~/vinculum", "~/vinculum/sub"] {
            let mut p = NewProject { name: "X".into(), folder: folder.into(), ..Default::default() };
            assert!(check(&mut p, &taken).unwrap_err().to_string().contains("overlaps"), "{folder}");
        }
        std::fs::write(tokens.path().join("viber-consultant.token"), "x").unwrap();
        let mut p = NewProject { name: "Viber Consultant".into(), folder: "~/vc-x".into(), ..Default::default() };
        assert!(check(&mut p, &taken).unwrap_err().to_string().contains("token"));
    }

    #[test]
    fn links_are_refused() {
        let home = crate::util::home_dir();
        let link = home.join(format!(".agentflow-test-link-{}", std::process::id()));
        std::os::unix::fs::symlink("/tmp", &link).unwrap();
        let r = check_folder(&format!("~/{}/x", link.file_name().unwrap().to_string_lossy()));
        std::fs::remove_file(&link).unwrap();
        assert!(r.unwrap_err().to_string().contains("is a link"));
    }
}
