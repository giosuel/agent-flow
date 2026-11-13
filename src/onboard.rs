//! Starting a project from the New project button: the record and an M0 milestone, its folder with
//! a local git repository, a project token in its file, and the agent's first note in its memory.
//! The owner, 2026-11-07: "i can create the repo, and then later on tell the agent to use the
//! remote", so nothing is created on GitHub. The agent's Claude Code setup (MCP server, hooks,
//! token variable) is written into the folder, kept out of git, so the owner only starts `claude`
//! there (the owner, 2026-11-12: "sets up all the agent stuff as well so we don't need the copy
//! paste commands"). Claude's own `~/.claude.json` is never touched.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension};

use crate::auth::{self, TokenKind};
use crate::projects::{self, NewMilestone, ProjectPatch};
use crate::util::expand_home;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Outcome {
    pub slug: String,
    pub steps: Vec<String>,
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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

/// The database part: project, details, M0, token. Returns the token's secret. Without a folder
/// (created on the page, set up later with `flow project setup`), the folder and machine come
/// when it's set up.
pub fn create_records(
    c: &mut Connection,
    p: &NewProject,
    folder: Option<&Path>,
    machine: Option<&str>,
    with_token: bool,
) -> anyhow::Result<(i64, Option<String>)> {
    let tx = c.transaction()?;
    let project = projects::create(&tx, &p.slug, p.name.trim(), "owner")?;
    let patch = ProjectPatch {
        description: Some(p.description.clone()),
        checkout: folder.map(|f| f.display().to_string()),
        machine: machine.map(str::to_owned),
        model: Some(p.model.clone()),
        effort: Some(p.effort.clone()),
        ..Default::default()
    };
    projects::update(&tx, project.id, &patch, "owner")?;
    // The stack and platform start the project document (the owner, 2026-11-12).
    let mut doc = String::new();
    if !p.stack.is_empty() {
        doc.push_str(&format!("## Tech stack\n\n{}\n\n", p.stack.join(" · ")));
    }
    if !p.platform.trim().is_empty() {
        doc.push_str(&format!("## Platform\n\n{}\n\n", p.platform.trim()));
    }
    if !doc.is_empty() {
        crate::projectdoc::set(&tx, project.id, &doc, "owner", "started with the project")?;
    }
    projects::add_milestone(
        &tx,
        project.id,
        &NewMilestone { name: "M0", title: "Getting started", description: "", parent: None, before: None },
        "owner",
    )?;
    let secret = match with_token {
        true => Some(auth::create_token(&tx, TokenKind::Project, p.name.trim(), Some(&p.slug))?.1),
        // Set up later: the setup code brings the token.
        false => None,
    };
    tx.commit()?;
    Ok((project.id, secret))
}

/// How long a setup code works.
pub const SETUP_CODE_MINUTES: i64 = 15;

/// A setup code for a project: what the page shows for `flow project setup` on a machine (the
/// owner, 2026-11-13). It's traded once for the project's token, so it's long enough not to be
/// guessed: twelve characters without look-alikes, about 59 bits, valid 15 minutes.
pub fn new_setup_code(c: &Connection, project_id: i64) -> anyhow::Result<String> {
    const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let raw = crate::util::random_bytes::<12>();
    let s: String = raw.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect();
    let code = format!("{}-{}-{}", &s[..4], &s[4..8], &s[8..]);
    let now = crate::util::now_ms();
    c.execute(
        "INSERT INTO setup_codes (hash, project_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![crate::util::hash_secret(&code), project_id, now, now + SETUP_CODE_MINUTES * 60_000],
    )?;
    Ok(code)
}

/// Trades a setup code for a new token of its project (once). Returns the project and the token.
pub fn redeem_setup_code(c: &mut Connection, code: &str, machine: &str) -> anyhow::Result<(projects::Project, String)> {
    let code = code.trim().to_uppercase();
    let tx = c.transaction()?;
    let now = crate::util::now_ms();
    let found: Option<i64> = tx
        .query_row(
            "UPDATE setup_codes SET used_at = ?1, used_by = ?2 WHERE hash = ?3 AND used_at IS NULL AND expires_at > ?1
             RETURNING project_id",
            rusqlite::params![now, machine, crate::util::hash_secret(&code)],
            |r| r.get(0),
        )
        .optional()?;
    let Some(project_id) = found else {
        bail!("that setup code is unknown, used or expired; make a new one on the project's page")
    };
    let p = projects::get_by_id(&tx, project_id)?;
    let (_, secret) = auth::create_token(&tx, TokenKind::Project, &format!("{} on {machine}", p.name), Some(&p.slug))?;
    tx.commit()?;
    Ok((p, secret))
}

/// The project is set up in this folder on this machine (what `flow project setup` reports).
pub fn mark_set_up(c: &Connection, project_id: i64, folder: &str, machine: &str) -> anyhow::Result<()> {
    let folder = folder.trim();
    if folder.is_empty() || !folder.starts_with('/') || folder.len() > 500 || folder.chars().any(char::is_control) {
        bail!("the folder must be an absolute path");
    }
    crate::approvals::check_machine(machine)?;
    let host = machine.rsplit('@').next().unwrap_or(machine);
    let patch =
        ProjectPatch { checkout: Some(folder.to_owned()), machine: Some(host.to_owned()), ..Default::default() };
    projects::update(c, project_id, &patch, "flow project setup")?;
    Ok(())
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
                 {name} is managed in AgentFlow ({public_url}), through the `agentflow` MCP server. A hook loads `hello` at \
                 the start of every session and after every compaction (call it yourself if it didn't come); record tasks, decisions and questions there, not in files \
                 in the repository. The owner's rules are in the guides (`guides`, `guide`). Ask the owner with \
                 `ask_question`; write to other agents and the consultant with Claude Code's SendMessage (AgentFlow logs it).\n",
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
    steps.push(write_setup(folder, &token_file, p.name.trim(), public_url)?);
    Ok(Outcome { slug: p.slug.clone(), steps, start: start_command(folder) })
}

/// The Claude Code hooks AgentFlow gives a project's sessions: hello (the document, then the
/// rest) on every start, resume, /clear and compaction, and the news before each of the owner's
/// prompts. Claude Code caps a hook's output at 10,000 characters, hence hello's two parts.
pub fn hook_commands(token_file: &Path, name: &str, public_url: &str) -> Vec<(&'static str, Vec<String>)> {
    let bin = bin();
    // The server and token are named, so the hooks don't depend on a config file being found.
    let tf = format!("{} --url {}", token_file.display(), shell_word(public_url.trim_end_matches('/')));
    let session = shell_word(name);
    vec![
        (
            "SessionStart",
            vec![
                format!("{bin} hello --doc --token-file {tf}"),
                format!("{bin} hello --no-doc --session {session} --token-file {tf}"),
            ],
        ),
        ("UserPromptSubmit", vec![format!("{bin} news --token-file {tf}")]),
        // The conversations reach AgentFlow from wherever the project lies (the owner, 2026-11-13).
        ("Stop", vec![format!("{bin} sync --token-file {tf}")]),
        ("SubagentStop", vec![format!("{bin} sync --token-file {tf}")]),
        ("SessionEnd", vec![format!("{bin} sync --token-file {tf}")]),
    ]
}

/// The `flow` program the hooks and the MCP sign-in call: installed next to the server.
fn bin() -> String {
    crate::util::flow_path().display().to_string()
}

/// AgentFlow's entry in the folder's `.mcp.json`: the token is read from its file whenever
/// Claude connects, never stored in a config.
fn mcp_server(token_file: &Path, public_url: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "http",
        "url": format!("{}/mcp", public_url.trim_end_matches('/')),
        "headersHelper": format!("{} mcp-headers --token-file {}", bin(), token_file.display()),
    })
}

/// A word for the shell: as it is when it's plain, single-quoted otherwise.
fn shell_word(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:/".contains(c)) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// One of AgentFlow's own hooks (any version of them: `flow hello` from before the split,
/// `flow hello` since), found by its command.
fn is_ours(command: &str) -> bool {
    let program = command.split_whitespace().next().unwrap_or("");
    let name = Path::new(program).file_name().and_then(|n| n.to_str()).unwrap_or("");
    matches!(name, "agentflow" | "flow")
        && command.contains("--token-file")
        && (command.contains(" hello ") || command.contains(" news ") || command.contains(" sync "))
}

fn settings_path(folder: &Path) -> PathBuf {
    folder.join(".claude").join("settings.local.json")
}

/// A JSON object from a file, or an empty one when there's no file; a file that isn't one is an
/// error, so it's never overwritten.
fn read_object(path: &Path) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    let v: serde_json::Value = match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text).with_context(|| {
            format!("{} isn't valid JSON, so it was left alone; fix it and try again", path.display())
        })?,
        Ok(_) => serde_json::json!({}),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    match v {
        serde_json::Value::Object(o) => Ok(o),
        _ => bail!("{} isn't a JSON object, so it was left alone", path.display()),
    }
}

fn write_json(path: &Path, v: serde_json::Map<String, serde_json::Value>) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&serde_json::Value::Object(v))? + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Sets the project's agent up in its folder, so the owner only has to start `claude` there (the
/// owner, 2026-11-12: "let's make it so agent-flow writes the hooks", then "sets up all the agent
/// stuff as well so we don't need the copy paste commands"):
/// - `.mcp.json`: the AgentFlow MCP server (its other servers stay);
/// - `.claude/settings.local.json`: the hooks, the server approved (no prompt), and
///   `AGENTFLOW_TOKEN_FILE` for the session's commands; AgentFlow's earlier hooks are replaced and
///   everything else stays.
///
/// Both stay out of git through `.git/info/exclude` (local, never committed). A `.mcp.json` the
/// repository commits is left alone. Claude's own `~/.claude.json` is never touched: running
/// sessions rewrite it all the time.
pub fn write_setup(folder: &Path, token_file: &Path, name: &str, public_url: &str) -> anyhow::Result<String> {
    use serde_json::{Map, Value, json};
    let mut did = Vec::new();
    // The MCP server.
    let mcp_path = folder.join(".mcp.json");
    if tracked(folder, ".mcp.json") {
        did.push(format!(
            "{} is committed in the repository, so AgentFlow's server wasn't added to it; add an \"agentflow\" entry there yourself",
            mcp_path.display()
        ));
    } else {
        let mut mcp = read_object(&mcp_path)?;
        let servers = mcp.entry("mcpServers").or_insert_with(|| json!({}));
        let Some(servers) = servers.as_object_mut() else {
            bail!("\"mcpServers\" in {} isn't an object, so it was left alone", mcp_path.display())
        };
        servers.insert("agentflow".into(), mcp_server(token_file, public_url));
        write_json(&mcp_path, mcp)?;
        did.push(format!(
            "the AgentFlow MCP server in {}{}",
            mcp_path.display(),
            keep_out_of_git(folder, ".mcp.json")?
        ));
    }
    // Hooks, approval, token variable.
    let path = settings_path(folder);
    let mut settings = read_object(&path)?;
    let hooks = settings.entry("hooks").or_insert_with(|| json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        bail!("\"hooks\" in {} isn't an object, so it was left alone", path.display())
    };
    // Drop AgentFlow's earlier hooks wherever they are, and groups left empty by that.
    for groups in hooks.values_mut().filter_map(Value::as_array_mut) {
        for g in groups.iter_mut() {
            if let Some(list) = g.get_mut("hooks").and_then(Value::as_array_mut) {
                list.retain(|h| !h.get("command").and_then(Value::as_str).is_some_and(is_ours));
            }
        }
        groups.retain(|g| g.get("hooks").and_then(Value::as_array).is_none_or(|l| !l.is_empty()));
    }
    hooks.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
    for (event, commands) in hook_commands(token_file, name, public_url) {
        let list: Vec<Value> =
            commands.into_iter().map(|c| json!({"type": "command", "command": c, "timeout": 15})).collect();
        let groups = hooks.entry(event).or_insert_with(|| json!([]));
        let Some(groups) = groups.as_array_mut() else {
            bail!("hooks.{event} in {} isn't a list, so it was left alone", path.display())
        };
        let mut group = Map::new();
        group.insert("hooks".into(), Value::Array(list));
        groups.push(Value::Object(group));
    }
    let approved = settings.entry("enabledMcpjsonServers").or_insert_with(|| json!([]));
    let Some(approved) = approved.as_array_mut() else {
        bail!("\"enabledMcpjsonServers\" in {} isn't a list, so it was left alone", path.display())
    };
    if !approved.iter().any(|v| v == "agentflow") {
        approved.push("agentflow".into());
    }
    let env = settings.entry("env").or_insert_with(|| json!({}));
    let Some(env) = env.as_object_mut() else {
        bail!("\"env\" in {} isn't an object, so it was left alone", path.display())
    };
    env.insert("AGENTFLOW_TOKEN_FILE".into(), token_file.display().to_string().into());
    env.insert("AGENTFLOW_URL".into(), public_url.trim_end_matches('/').into());
    write_json(&path, settings)?;
    did.push(format!(
        "in {}: the hooks (hello on every start, resume, /clear and compaction; the news before each of your prompts; the conversations sent after each reply), the server approved, and AGENTFLOW_TOKEN_FILE and AGENTFLOW_URL for its `flow` commands{}",
        path.display(),
        keep_out_of_git(folder, ".claude/settings.local.json")?
    ));
    Ok(format!("Set the agent up: {}.", did.join("; ")))
}

/// Whether the folder is set up as [`write_setup`] would set it up now.
pub fn setup_present(folder: &Path, token_file: &Path, name: &str, public_url: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(settings_path(folder)) else { return false };
    let Ok(settings) = serde_json::from_str::<serde_json::Value>(&text) else { return false };
    let hooks = hook_commands(token_file, name, public_url)
        .iter()
        .flat_map(|(_, c)| c)
        .all(|c| text.contains(&serde_json::to_string(c).unwrap_or_default()));
    let approved = settings["enabledMcpjsonServers"].as_array().is_some_and(|a| a.iter().any(|v| v == "agentflow"));
    let env = settings["env"]["AGENTFLOW_TOKEN_FILE"].as_str() == Some(&token_file.display().to_string())
        && settings["env"]["AGENTFLOW_URL"].as_str() == Some(public_url.trim_end_matches('/'));
    let mcp = std::fs::read_to_string(folder.join(".mcp.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .is_some_and(|m| m["mcpServers"]["agentflow"] == mcp_server(token_file, public_url));
    hooks && approved && env && mcp
}

/// Whether git tracks this file in the folder's repository.
fn tracked(folder: &Path, rel: &str) -> bool {
    folder.join(".git").exists()
        && std::process::Command::new("git")
            .arg("-C")
            .arg(folder)
            .args(["ls-files", "--error-unmatch", rel])
            .output()
            .is_ok_and(|o| o.status.success())
}

/// Makes git ignore a file in the folder's repository, through `.git/info/exclude` (local, never
/// committed). Returns a few words for the step.
fn keep_out_of_git(folder: &Path, rel: &str) -> anyhow::Result<String> {
    if !folder.join(".git").exists() {
        return Ok(String::new());
    }
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(folder)
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(args)
            .output()
    };
    let ignored = git(&["check-ignore", "-q", rel]).context("running git check-ignore")?;
    if ignored.status.success() {
        return Ok(" (git ignores it)".into());
    }
    let out = git(&["rev-parse", "--git-path", "info/exclude"]).context("running git rev-parse")?;
    if !out.status.success() {
        bail!("git rev-parse: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let exclude = if Path::new(&path).is_absolute() { PathBuf::from(path) } else { folder.join(path) };
    let mut text = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("/{rel}\n"));
    if let Some(dir) = exclude.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&exclude, text)?;
    Ok(" (added to .git/info/exclude, so git ignores it without a commit)".into())
}

/// The owner's one command for a project's folder: start the agent there. Everything else is
/// set up by [`write_setup`]. Shown after New project, and on the project's page afterwards.
pub fn start_command(folder: &Path) -> String {
    format!("cd {} && claude", folder.display())
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
    fn hooks_from_before_and_after_the_split_are_ours() {
        assert!(is_ours("/home/v/.local/bin/agentflow hello --doc --token-file /t --url https://f"));
        assert!(is_ours("/home/v/.local/bin/flow news --token-file /t"));
        assert!(is_ours("flow hello --no-doc --session X --token-file /t"));
        assert!(!is_ours("/usr/bin/notflow hello --token-file /t"));
        assert!(is_ours("/home/v/.local/bin/flow sync --token-file /t"));
        assert!(!is_ours("echo agentflow hello --token-file"));
    }

    #[test]
    fn a_project_starts_with_its_folder_token_and_note() {
        assert_eq!(slug_for("Speculum Tool!"), "speculum-tool");
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("speculum");
        let p = NewProject { name: "Speculum".into(), slug: "speculum".into(), ..Default::default() };
        let db = crate::db::Db::open_memory();
        let f = folder.clone();
        let (id, secret) = db.call_blocking(move |c| create_records(c, &p, Some(&f), Some("box"), true)).unwrap();
        let secret = secret.unwrap();
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
        assert_eq!(out.start, format!("cd {} && claude", folder.display()));
        let mcp = std::fs::read_to_string(folder.join(".mcp.json")).unwrap();
        assert!(mcp.contains("https://flow.example/mcp") && mcp.contains("tokens/speculum.token"), "{mcp}");
        assert!(!mcp.contains(&secret), "the config never holds the token");
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
        assert!(
            setup_present(&folder, &home.path().join("tokens/speculum.token"), "Speculum", "x"),
            "set up again with the last URL"
        );
    }

    #[test]
    fn the_setup_is_written_beside_the_settings_already_there() {
        let home = tempfile::tempdir().unwrap();
        let folder = home.path().join("office");
        std::fs::create_dir_all(folder.join(".claude")).unwrap();
        std::process::Command::new("git").arg("-C").arg(&folder).args(["init", "-q"]).output().unwrap();
        let token = home.path().join("tokens/imaginarium.token");
        let settings = folder.join(".claude/settings.local.json");
        // Someone's own settings and hooks, and an older AgentFlow hook.
        std::fs::write(
            &settings,
            r#"{"permissions": {"allow": ["Bash(ls)"]}, "hooks": {"UserPromptSubmit": [{"hooks": [
                {"type": "command", "command": "/old/flow news --token-file /x.token"},
                {"type": "command", "command": "echo mine"}]}], "Stop": [{"hooks": [{"type": "command", "command": "beep"}]}]}}"#,
        )
        .unwrap();
        std::fs::write(folder.join(".mcp.json"), r#"{"mcpServers": {"other": {"type": "stdio", "command": "x"}}}"#)
            .unwrap();
        let url = "https://flow.example";
        assert!(!setup_present(&folder, &token, "Imaginarium", url));
        let step = write_setup(&folder, &token, "Imaginarium", url).unwrap();
        assert!(step.contains("info/exclude"), "{step}");
        write_setup(&folder, &token, "Imaginarium", url).unwrap();
        assert!(setup_present(&folder, &token, "Imaginarium", url));
        assert!(!setup_present(&folder, &token, "Imaginarium", "https://elsewhere.example"));
        let mcp: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(folder.join(".mcp.json")).unwrap()).unwrap();
        assert_eq!(mcp["mcpServers"]["other"]["command"], "x");
        assert_eq!(mcp["mcpServers"]["agentflow"]["url"], "https://flow.example/mcp");
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(v["permissions"]["allow"][0], "Bash(ls)");
        assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "beep");
        let text = v.to_string();
        assert!(text.contains("echo mine") && !text.contains("/old/agentflow"), "{text}");
        assert_eq!(text.matches("hello --doc").count(), 1, "written twice, present once: {text}");
        assert_eq!(text.matches(" news --token-file").count(), 1, "{text}");
        assert!(text.contains("--session Imaginarium"));
        assert_eq!(v["enabledMcpjsonServers"], serde_json::json!(["agentflow"]), "approved once: {text}");
        assert_eq!(v["env"]["AGENTFLOW_TOKEN_FILE"], token.display().to_string());
        assert_eq!(v["env"]["AGENTFLOW_URL"], url.trim_end_matches('/'));
        for f in [".claude/settings.local.json", ".mcp.json"] {
            let ignored = std::process::Command::new("git")
                .arg("-C")
                .arg(&folder)
                .args(["check-ignore", "-q", f])
                .status()
                .unwrap();
            assert!(ignored.success(), "{f}");
        }
        let ignored = std::process::Command::new("git")
            .arg("-C")
            .arg(&folder)
            .args(["check-ignore", "-q", ".claude/settings.local.json"])
            .status()
            .unwrap();
        assert!(ignored.success());
        assert!(write_setup(&folder, &token, "Imaginarium", url).unwrap().contains("git ignores it"));
        // A file that isn't JSON is left alone.
        std::fs::write(&settings, "{ not json").unwrap();
        assert!(write_setup(&folder, &token, "Imaginarium", url).is_err());
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "{ not json");
        assert_eq!(shell_word("Cold Trace"), "'Cold Trace'");
        // A .mcp.json the repository commits is left alone, and the step says so.
        let other = home.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&other)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        std::fs::write(other.join(".mcp.json"), "{}\n").unwrap();
        git(&["add", ".mcp.json"]);
        git(&["commit", "-q", "-m", "x"]);
        let step = write_setup(&other, &token, "Other", url).unwrap();
        assert!(step.contains("is committed in the repository"), "{step}");
        assert_eq!(std::fs::read_to_string(other.join(".mcp.json")).unwrap(), "{}\n");
        assert!(!setup_present(&other, &token, "Other", url));
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
