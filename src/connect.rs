//! `flow connect`, `disconnect`, `status`, `update`, the owner's admin jobs (`flow token`,
//! `project`, `backup`, `import-prs`) and `flow server add-passkey` (the owner, 2026-11-13).
//!
//! Connecting and the admin jobs wait for the owner's approval on the page (see
//! [`crate::approvals`]): flow shows a link and a short code, opens the browser when it can, and
//! polls until the owner approves or denies.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde_json::{Value, json};

use crate::cli::{Client, ClientArgs, connection_file, server_url};
use crate::util::expand_home;

/// Where `flow connect` keeps the machine's token.
pub fn token_file() -> PathBuf {
    expand_home("~/.config/agentflow/token")
}

/// This machine as the owner sees it on the approval page: "viber@mothman".
pub fn machine_name() -> String {
    let user = std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| rustix::process::getuid().as_raw().to_string());
    format!("{user}@{}", crate::machine::hostname())
}

/// Opens a link in the browser when this machine has a desktop; on a server it doesn't, and the
/// link and code are printed for the owner to open on their phone.
fn open_browser(url: &str) -> bool {
    let desktop = std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
    if !desktop {
        return false;
    }
    std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/// Asks for something the owner approves on the page, and waits for their answer. Returns the
/// result the server hands over once (e.g. a new token).
pub fn approve(client: &Client, action: &str, args: Value) -> anyhow::Result<Value> {
    let asked =
        client.post_json("/api/approvals", &json!({"action": action, "args": args, "machine": machine_name()}))?;
    let code = asked["code"].as_str().context("no code in the answer")?.to_owned();
    let url = asked["url"].as_str().context("no link in the answer")?.to_owned();
    let poll = asked["poll"].as_str().context("no poll secret in the answer")?.to_owned();
    let wait = Duration::from_secs(asked["expires_in"].as_u64().unwrap_or(600) + 5);
    let opened = open_browser(&url);
    {
        use crate::ui::{Tone, paint};
        let on = crate::ui::color(true);
        eprintln!("Approve this in your browser{}:", if opened { " (opened)" } else { "" });
        eprintln!("  {}", paint(Tone::Number, &url, on));
        eprintln!("The page shows the code {}; approve only if it matches.", paint(Tone::Bold, &code, on));
    }
    let anon = Client::anonymous(&client.base);
    let start = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let got = match anon.post_json("/api/approvals/poll", &json!({"poll": poll})) {
            Ok(v) => v,
            // A dropped connection isn't an answer: try again until the request expires.
            Err(_) if start.elapsed() < wait => continue,
            Err(e) => return Err(e),
        };
        match got["state"].as_str().unwrap_or("") {
            "pending" if start.elapsed() < wait => continue,
            "pending" | "expired" => {
                bail!("nobody approved it within {} minutes; run it again", crate::approvals::MINUTES)
            }
            "approved" => return Ok(got["result"].clone()),
            "denied" => bail!("denied on the page; nothing was done"),
            "failed" => bail!("approved, but it failed: {}", got["error"].as_str().unwrap_or("no reason given")),
            "done" => bail!("this was approved and handed out already"),
            other => bail!("unexpected answer {other:?}"),
        }
    }
}

/// Writes a token where only this user can read it, replacing the old one in one step.
pub fn write_secret(path: &Path, secret: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().context("a token file needs a folder")?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.new", path.file_name().and_then(|n| n.to_str()).unwrap_or("token")));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    writeln!(f, "{secret}")?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn connect(url: &str) -> anyhow::Result<()> {
    let url = url.trim().trim_end_matches('/');
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        bail!("give the server's address, e.g. flow connect https://flow.example");
    }
    let tf = token_file();
    if tf.exists() && connection_file().exists() {
        let saved = server_url(Path::new("/nonexistent"), &ClientArgs::default()).unwrap_or_default();
        // The install command run again: nothing to do.
        if saved.trim_end_matches('/') == url {
            eprintln!("This machine is connected to {url} already.");
            return Ok(());
        }
        bail!(
            "this machine is connected already ({saved}); see `flow status`, or `flow disconnect` first to connect it anew"
        );
    }
    let client = Client::anonymous(url);
    let result = approve(&client, "connect", json!({}))?;
    let token = result["token"].as_str().context("the server sent no token")?;
    write_secret(&tf, token)?;
    std::fs::write(connection_file(), format!("url = {}\n", toml::Value::String(url.to_owned())))?;
    println!("Connected to {url} as {}.", result["name"].as_str().unwrap_or("this machine"));
    println!("Try it: flow status, or flow run to list the test builds.");
    Ok(())
}

pub fn disconnect(config_path: &Path) -> anyhow::Result<()> {
    let tf = token_file();
    if !tf.exists() {
        bail!("this machine isn't connected (no {})", tf.display());
    }
    let revoked = Client::new(config_path, &ClientArgs::default()).and_then(|c| c.delete("/api/tokens/self"));
    let _ = std::fs::remove_file(&tf);
    let _ = std::fs::remove_file(connection_file());
    match revoked {
        Ok(_) => println!("Disconnected: the token is revoked on the server and removed here."),
        Err(e) => println!(
            "Removed the token here, but the server couldn't revoke it ({e:#}). Revoke it in AgentFlow's Settings."
        ),
    }
    Ok(())
}

pub fn status(config_path: &Path, args: &ClientArgs) -> anyhow::Result<()> {
    use crate::ui::{Tone, paint, wrap};
    let on = crate::ui::color(false);
    let key = |k: &str| paint(Tone::Dim, &format!("{k:<9}"), on);
    let mine = env!("APP_VERSION");
    println!("{}{}", key("flow"), paint(Tone::Bold, mine, on));
    let url = match server_url(config_path, args) {
        Ok(u) => u,
        Err(e) => {
            println!("{}{}", key("server"), paint(Tone::Bad, "not connected", on));
            println!("{}", wrap(9, Tone::Dim, &format!("{e:#}"), on));
            return Ok(());
        }
    };
    println!("{}{url}", key("server"));
    let tf = args.token_file.clone().unwrap_or_else(token_file);
    println!("{}{}", key("token"), paint(Tone::Dim, &tf.display().to_string(), on));
    let client = match Client::new(config_path, args) {
        Ok(c) => c,
        Err(e) => {
            println!("{}", wrap(9, Tone::Bad, &format!("{e:#}"), on));
            return Ok(());
        }
    };
    match client.get("/api/status") {
        Ok(s) => {
            let theirs = s["version"].as_str().unwrap_or("?");
            if theirs == mine {
                println!("{}{} {}", key("version"), theirs, paint(Tone::Good, "●", on));
            } else {
                println!(
                    "{}{}  {}",
                    key("version"),
                    theirs,
                    paint(Tone::Commit, "differs from flow's: run flow update", on)
                );
            }
            println!(
                "{}{} {}",
                key("as"),
                paint(Tone::Bold, s["who"]["name"].as_str().unwrap_or("?"), on),
                paint(Tone::Dim, &format!("({})", s["who"]["kind"].as_str().unwrap_or("?")), on)
            );
        }
        Err(e) => println!("{}{}", key(""), paint(Tone::Bad, &format!("unreachable: {e:#}"), on)),
    }
    Ok(())
}

/// Replaces this `flow` with the one the server hands out, checked against its checksum.
pub fn update(config_path: &Path, args: &ClientArgs) -> anyhow::Result<()> {
    let url = server_url(config_path, args)?;
    let client = Client::anonymous(&url).with_timeout(Duration::from_secs(600));
    let me = std::env::current_exe().context("finding this program")?;
    let dir = me.parent().context("this program's folder")?;
    let tmp = dir.join(".flow.update");
    client.download("/download/flow", &tmp)?;
    let want = client_text(&client, "/download/flow.sha256")?;
    let have = crate::builds::sha256_of(std::fs::File::open(&tmp)?)?.0;
    if want.split_whitespace().next() != Some(have.as_str()) {
        let _ = std::fs::remove_file(&tmp);
        bail!("the download doesn't match its checksum; nothing was changed");
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    let new = std::process::Command::new(&tmp).arg("--version").output()?;
    std::fs::rename(&tmp, &me)?;
    println!(
        "Updated {}: {} → {}",
        me.display(),
        env!("APP_VERSION"),
        String::from_utf8_lossy(&new.stdout).trim().trim_start_matches("flow ")
    );
    Ok(())
}

fn client_text(client: &Client, path: &str) -> anyhow::Result<String> {
    let tmp = tempfile::NamedTempFile::new()?;
    client.download(path, tmp.path())?;
    Ok(std::fs::read_to_string(tmp.path())?)
}

/// `flow token create project <project> [--file]`, `consultant`; list; revoke; rotate.
pub fn token_create(
    config_path: &Path,
    args: &ClientArgs,
    kind: &str,
    project: Option<&str>,
    name: Option<&str>,
    file: Option<&Path>,
) -> anyhow::Result<()> {
    let client = Client::new(config_path, args)?;
    let kind = if kind == "consultant" { "admin" } else { kind };
    let result = approve(
        &client,
        "token_create",
        json!({"kind": kind, "project": project.unwrap_or(""), "name": name.unwrap_or("")}),
    )?;
    hand_over_token(&result, file)
}

fn hand_over_token(result: &Value, file: Option<&Path>) -> anyhow::Result<()> {
    let token = result["token"].as_str().context("the server sent no token")?;
    match file {
        Some(f) => {
            write_secret(f, token)?;
            println!(
                "Token {} ({}) written to {} (only you can read it).",
                result["id"],
                result["name"].as_str().unwrap_or(""),
                f.display()
            );
        }
        None => {
            println!("{token}");
            eprintln!(
                "Token {} ({}): shown once; AgentFlow keeps only its hash.",
                result["id"],
                result["name"].as_str().unwrap_or("")
            );
        }
    }
    Ok(())
}

pub fn token_list(config_path: &Path, args: &ClientArgs) -> anyhow::Result<()> {
    let client = Client::new(config_path, args)?;
    let list = client.get("/api/tokens")?;
    for t in list["tokens"].as_array().cloned().unwrap_or_default() {
        if !t["revoked_at"].is_null() {
            continue;
        }
        println!(
            "{:>4}  {:<8} {:<28} {:<14} …{}",
            t["id"],
            t["kind"].as_str().unwrap_or(""),
            t["name"].as_str().unwrap_or(""),
            t["project"].as_str().unwrap_or("-"),
            t["hint"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

pub fn token_revoke(config_path: &Path, args: &ClientArgs, id: i64) -> anyhow::Result<()> {
    let client = Client::new(config_path, args)?;
    approve(&client, "token_revoke", json!({"id": id}))?;
    println!("Token {id} is revoked.");
    Ok(())
}

pub fn token_rotate(config_path: &Path, args: &ClientArgs, id: i64, file: Option<&Path>) -> anyhow::Result<()> {
    let client = Client::new(config_path, args)?;
    let result = approve(&client, "token_rotate", json!({"id": id}))?;
    hand_over_token(&result, file)
}

pub fn project_add(config_path: &Path, args: &ClientArgs, slug: &str, name: &str) -> anyhow::Result<()> {
    let client = Client::new(config_path, args)?;
    approve(&client, "project_add", json!({"slug": slug, "name": name}))?;
    println!("Project {slug} added.");
    Ok(())
}

pub fn project_list(config_path: &Path, args: &ClientArgs) -> anyhow::Result<()> {
    let client = Client::new(config_path, args)?;
    let list = client.get("/api/projects")?;
    for p in list["projects"].as_array().cloned().unwrap_or_default() {
        println!(
            "{:<16} {:<24} {:<8} {}",
            p["slug"].as_str().unwrap_or(""),
            p["name"].as_str().unwrap_or(""),
            p["status"].as_str().unwrap_or(""),
            p["checkout"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

pub fn backup(config_path: &Path, args: &ClientArgs) -> anyhow::Result<()> {
    let client = Client::new(config_path, args)?;
    let r = approve(&client, "backup", json!({}))?;
    match r["backup"].as_str() {
        Some(p) => println!("Backed up to {p} on the server; exported {} projects.", r["exported"]),
        None => println!("Exported {} projects (today's backup existed already).", r["exported"]),
    }
    Ok(())
}

pub fn import_prs(config_path: &Path, args: &ClientArgs, project: &str, range: &str) -> anyhow::Result<()> {
    let (from, to) = range
        .split_once(['-', '–'])
        .and_then(|(a, b)| {
            Some((
                a.trim().trim_start_matches('#').parse::<i64>().ok()?,
                b.trim().trim_start_matches('#').parse::<i64>().ok()?,
            ))
        })
        .context("give a range of PR numbers, e.g. 300-374")?;
    let client = Client::new(config_path, args)?;
    let r = approve(&client, "import_prs", json!({"project": project, "from": from, "to": to}))?;
    println!(
        "Imported {} PRs into {project}; {} task links by branch.",
        r["imported"].as_array().map(Vec::len).unwrap_or(0),
        r["linked"]
    );
    Ok(())
}

/// The sockets a running server may listen on for `flow server add-passkey`: a system install's,
/// then a user install's.
fn admin_sockets() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from(crate::config::SYSTEM_ADMIN_SOCKET)];
    if let Ok(rd) = std::fs::read_dir("/run/user") {
        let mut users: Vec<PathBuf> = rd.flatten().map(|e| e.path().join("agentflow/admin.sock")).collect();
        users.sort();
        v.extend(users);
    }
    v
}

/// Asks the server on this machine over its local socket, which only root may use (D-273, D-274).
fn admin_request(socket: Option<&Path>, req: Value) -> anyhow::Result<Value> {
    use std::io::{BufRead, BufReader};
    let candidates = match socket {
        Some(s) => vec![s.to_path_buf()],
        None => admin_sockets(),
    };
    let mut tried = Vec::new();
    for path in candidates {
        let mut stream = match std::os::unix::net::UnixStream::connect(&path) {
            Ok(s) => s,
            Err(e) => {
                if path.exists() {
                    tried.push(format!("{}: {e}", path.display()));
                }
                continue;
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        writeln!(stream, "{req}")?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line)?;
        let v: Value = serde_json::from_str(&line).context("a broken answer from the server")?;
        if let Some(e) = v["error"].as_str() {
            bail!("{e}");
        }
        return Ok(v);
    }
    if tried.is_empty() {
        bail!("no AgentFlow server is running on this machine (no admin socket found)");
    }
    bail!("couldn't reach the server's socket: {} (run it with sudo)", tried.join("; "))
}

/// "15 minutes" when it's that, otherwise "until Wed 2 Dec 15:40".
fn works_until(v: &Value) -> String {
    let at = v["expires_at"].as_i64().unwrap_or(0);
    let left = at - crate::util::now_ms();
    if left <= 16 * 60_000 {
        "for 15 minutes".into()
    } else {
        let tz = crate::tz::Tz::default();
        format!("until {} {}", tz.day_label(at).split(' ').next().unwrap_or(""), tz.short(at))
    }
}

/// `sudo flow server setup`: a link that makes the admin, or makes the admin anew when they lost
/// their passkeys (their passkeys go and their browsers are signed out when it's used).
pub fn server_setup(socket: Option<&Path>) -> anyhow::Result<()> {
    let status = admin_request(socket, json!({"request": "status"}))?;
    let at = status["url"].as_str().unwrap_or("this server").to_owned();
    let mut confirm = String::new();
    if let Some(admin) = status["admin"].as_object() {
        let name = admin["name"].as_str().unwrap_or("the admin");
        let others = status["people"].as_i64().unwrap_or(1) - 1;
        println!(
            "This AgentFlow already has an admin: {name} ({} passkey{}, {} signed-in browser{}).",
            admin["passkeys"],
            if admin["passkeys"] == 1 { "" } else { "s" },
            admin["browsers"],
            if admin["browsers"] == 1 { "" } else { "s" }
        );
        println!("Setting it up again makes the admin anew: {name}'s passkeys are removed");
        println!(
            "and their browsers signed out when the link is used. Projects, records and the {others} other {} stay.",
            if others == 1 { "person" } else { "people" }
        );
        let prompt =
            if admin["username"].is_string() { "Type the admin's username to go on" } else { "Type yes to go on" };
        confirm = ask(prompt, "")?;
    } else {
        println!("AgentFlow at {at} has no admin yet.");
    }
    let v = admin_request(socket, json!({"request": "setup", "confirm": confirm}))?;
    if v["again"].as_bool() == Some(true) {
        println!("Open this link to make a new passkey for the admin.");
    } else {
        println!("Open this link to make the admin account and its first passkey.");
    }
    println!("It works once, {}:", works_until(&v));
    println!("  {}", v["url"].as_str().unwrap_or(""));
    if v["again"].as_bool() != Some(true) {
        println!("Then you pick your username, email and icon.");
    }
    Ok(())
}

/// `sudo flow server invite`: a link for someone new, from the admin; it works once, for 7 days.
pub fn server_invite(socket: Option<&Path>) -> anyhow::Result<()> {
    let v = admin_request(socket, json!({"request": "invite"}))?;
    println!("Send this link to the person you're inviting.");
    println!("It works once, {}:", works_until(&v));
    println!("  {}", v["url"].as_str().unwrap_or(""));
    println!("They make a passkey there and pick their name and icon.");
    println!("Settings › People lists it until it's used, with Withdraw.");
    Ok(())
}

/// `sudo flow server add-device [username]`: a link that adds a passkey for a new device to that
/// person's account (the admin's if no one is named).
pub fn server_add_device(socket: Option<&Path>, username: Option<&str>) -> anyhow::Result<()> {
    let v = admin_request(socket, json!({"request": "add_device", "username": username.unwrap_or("")}))?;
    let who = v["person"].as_str().unwrap_or("");
    let admin = if v["admin"].as_bool() == Some(true) { " (the admin)" } else { "" };
    println!("Open this link on the new device to add a passkey to {who}{admin}.");
    println!("It works once, {}:", works_until(&v));
    println!("  {}", v["url"].as_str().unwrap_or(""));
    if username.is_none() {
        println!("For someone else: sudo flow server add-device <username>");
    }
    Ok(())
}

/// Asks a question on the terminal; Enter takes the default.
fn ask(question: &str, default: &str) -> anyhow::Result<String> {
    use std::io::BufRead;
    if default.is_empty() {
        eprint!("{question}: ");
    } else {
        eprint!("{question} [{default}]: ");
    }
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let line = line.trim();
    Ok(if line.is_empty() { default.to_owned() } else { line.to_owned() })
}

/// Reads a line without showing it (the setup code).
fn ask_hidden(question: &str) -> anyhow::Result<String> {
    use rustix::termios::{LocalModes, OptionalActions, tcgetattr, tcsetattr};
    let stdin = std::io::stdin();
    let before = tcgetattr(&stdin).ok();
    if let Some(t) = &before {
        let mut quiet = t.clone();
        quiet.local_modes.remove(LocalModes::ECHO);
        let _ = tcsetattr(&stdin, OptionalActions::Now, &quiet);
    }
    let got = ask(question, "");
    if let Some(t) = &before {
        let _ = tcsetattr(&stdin, OptionalActions::Now, t);
        eprintln!();
    }
    got
}

/// The GitHub repository of a folder's `origin`, as owner/name, if it's on GitHub.
fn github_repo(folder: &Path) -> Option<String> {
    let out =
        std::process::Command::new("git").arg("-C").arg(folder).args(["remote", "get-url", "origin"]).output().ok()?;
    let url = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let repo = rest.trim_end_matches('/').trim_end_matches(".git").to_owned();
    crate::projects::looks_like_repo(&repo).then_some(repo)
}

/// "speculum-tool" → "Speculum Tool"
fn title_of(name: &str) -> String {
    name.split(['-', '_', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `flow project setup [folder]` (the owner, 2026-11-13): sets a project up in this folder (or the
/// one given), with a setup code from its page, or, with none, creates it (with the owner's
/// approval). Writes the folder's git repository, the project's token, the agent's first memory
/// note and its Claude Code setup, tells the server where it lies, and sends the folder's earlier
/// conversations once. Running it again changes nothing that's there and sends nothing twice.
pub fn project_setup(
    config_path: &Path,
    args: &ClientArgs,
    folder: Option<PathBuf>,
    code: Option<String>,
    name: Option<String>,
) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    let cwd = std::env::current_dir()?;
    let folder = match folder {
        Some(f) if f.is_absolute() => f,
        Some(f) => cwd.join(f),
        None => cwd,
    };
    let home = crate::util::home_dir();
    if folder == home || [".config", ".claude", ".local", ".ssh"].iter().any(|d| folder.starts_with(home.join(d))) {
        bail!("{} can't be a project's folder; use one of its own, e.g. ~/speculum", folder.display());
    }
    let interactive = std::io::stdin().is_terminal();
    // Set up before: refresh what's there, with the token it has; nothing new is created.
    if code.is_none()
        && let Some((url, token)) = existing_setup(&folder)
        && let Ok(st) = Client::with_token(&url, &token).get("/api/status")
        && st["who"]["kind"] == "project"
    {
        let slug = s(&st["who"]["name"]);
        println!("{} is set up already as {slug} on {url}; refreshing its setup.", folder.display());
        // The name its hooks give the session stays (Imaginarium's agent, 2026-11-18: a refresh
        // had turned "--session Imaginarium" into the slug).
        let name = session_name(&folder).unwrap_or_else(|| slug.clone());
        return finish_setup(&folder, &url, &slug, &name, &token, &machine_name());
    }
    let code = match code {
        Some(c) => c,
        None if interactive => ask_hidden("Setup code (or press Enter to create a new project)")?,
        None => String::new(),
    };
    let machine = machine_name();
    let (url, slug, project_name, token) = if !code.trim().is_empty() {
        let url = match &args.url {
            Some(u) => u.clone(),
            None => server_url(config_path, args)
                .context("which server? pass --url <server> (the page shows the command)")?,
        };
        let got = Client::anonymous(&url)
            .post_json("/api/setup/redeem", &json!({"code": code.trim(), "machine": machine}))?;
        (url, s(&got["project"]), s(&got["name"]), s(&got["token"]))
    } else {
        let client = Client::new(config_path, args)
            .context("creating a project needs a connected machine: flow connect <server> first")?;
        let base = folder.file_name().and_then(|n| n.to_str()).unwrap_or("project");
        let default = title_of(base);
        let project_name = match name {
            Some(n) => n,
            None if interactive => {
                ask(&format!("New project \"{}\": name", crate::onboard::slug_for(&default)), &default)?
            }
            None => default,
        };
        let slug = crate::onboard::slug_for(&project_name);
        let repo = github_repo(&folder).unwrap_or_default();
        let got = approve(
            &client,
            "project_create",
            json!({"name": project_name, "slug": slug, "folder": folder.display().to_string(), "repo": repo}),
        )?;
        eprintln!("{}", crate::ui::ok("approved"));
        (client.base.clone(), s(&got["project"]), s(&got["name"]), s(&got["token"]))
    };
    if token.is_empty() || slug.is_empty() {
        bail!("the server sent no project or token");
    }
    println!("{project_name} ({slug}) on {url}");
    finish_setup(&folder, &url, &slug, &project_name, &token, &machine)
}

/// The token file and server a folder's setup names (`.claude/settings.local.json`).
pub(crate) fn existing_setup(folder: &Path) -> Option<(String, String)> {
    let text = std::fs::read_to_string(folder.join(".claude/settings.local.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let token_file = v["env"]["AGENTFLOW_TOKEN_FILE"].as_str()?;
    // Set up before AGENTFLOW_URL was written there: the MCP server's address says it.
    let url = match v["env"]["AGENTFLOW_URL"].as_str() {
        Some(u) => u.to_owned(),
        None => {
            let mcp: Value = serde_json::from_str(&std::fs::read_to_string(folder.join(".mcp.json")).ok()?).ok()?;
            mcp["mcpServers"]["agentflow"]["url"].as_str()?.trim_end_matches('/').trim_end_matches("/mcp").to_owned()
        }
    };
    let token = std::fs::read_to_string(token_file).ok()?.trim().to_owned();
    (!token.is_empty()).then_some((url, token))
}

/// The session name a folder's AgentFlow hooks give (`flow hello --session <name>`), if they do.
fn session_name(folder: &Path) -> Option<String> {
    let text = std::fs::read_to_string(folder.join(".claude/settings.local.json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    session_name_in(&v)
}

fn session_name_in(v: &Value) -> Option<String> {
    let groups = v["hooks"]["SessionStart"].as_array()?;
    let command = groups
        .iter()
        .flat_map(|g| g["hooks"].as_array().into_iter().flatten())
        .filter_map(|h| h["command"].as_str())
        .find(|c| c.contains(" hello ") && c.contains(" --session "))?;
    let rest: Vec<char> = command.split(" --session ").nth(1)?.chars().collect();
    // One shell word, as onboard::shell_word writes it: plain, or in single quotes with '\'' for a
    // quote inside.
    let (mut name, mut i) = (String::new(), 0);
    while i < rest.len() {
        match rest[i] {
            '\'' => {
                i += 1;
                while i < rest.len() && rest[i] != '\'' {
                    name.push(rest[i]);
                    i += 1;
                }
                i += 1;
            }
            '\\' if i + 1 < rest.len() => {
                name.push(rest[i + 1]);
                i += 2;
            }
            c if c.is_whitespace() => break,
            c => {
                name.push(c);
                i += 1;
            }
        }
    }
    (!name.is_empty()).then_some(name)
}

/// The local part of a setup: folder, git, token file, first note, Claude Code setup; then where
/// it lies, and the folder's earlier conversations (only what the server doesn't have yet).
fn finish_setup(
    folder: &Path,
    url: &str,
    slug: &str,
    project_name: &str,
    token: &str,
    machine: &str,
) -> anyhow::Result<()> {
    let p = crate::onboard::NewProject { name: project_name.to_owned(), slug: slug.to_owned(), ..Default::default() };
    let token_dir = expand_home("~/.config/agentflow/tokens");
    let outcome = crate::onboard::create_files(&p, folder, token, &token_dir, &crate::sync::transcripts_root(), url)?;
    for step in &outcome.steps {
        println!("  ✓ {step}");
    }
    let client = Client::with_token(url, token);
    client.post_json("/api/setup/done", &json!({"folder": folder.display().to_string(), "machine": machine}))?;
    // The handbook: a copy on this machine, and Claude Code's global CLAUDE.md including it (the
    // owner, 2026-11-18, D-209).
    match crate::cli::sync_handbook(&client) {
        Ok(_) => println!("  ✓ the handbook's copy is in {}", crate::cli::handbook_dir().display()),
        Err(e) => println!("  ! the handbook's copy wasn't made: {e:#}"),
    }
    if crate::cli::handbook_dir().join("CLAUDE.md").exists() {
        match include_handbook(&crate::util::home_dir()) {
            Ok(Some(what)) => println!("  ✓ {what}"),
            Ok(None) => {}
            Err(e) => println!("  ! ~/.claude/CLAUDE.md wasn't changed: {e:#}"),
        }
    }
    let (n, _) = crate::sync::session_count(folder);
    if n > 0 {
        let files = crate::sync::conversations(folder);
        let calls = crate::sync::send(&client.with_timeout(Duration::from_secs(300)), &files)?;
        let plural = if n == 1 { "conversation" } else { "conversations" };
        if calls > 0 {
            println!("  ✓ sent the folder's {n} earlier {plural} to AgentFlow ({calls} calls it didn't have)");
        } else {
            println!("  ✓ AgentFlow has the folder's {n} earlier {plural} already");
        }
        println!();
        println!("This folder already has {n} Claude Code conversation{}.", if n == 1 { "" } else { "s" });
        println!("If its agent keeps the project's plans, decisions or tasks in its own files,");
        println!("you can now tell it to move them into AgentFlow.");
    }
    println!();
    println!("The project has been set up.");
    Ok(())
}

/// Makes `~/.claude/CLAUDE.md` include the handbook's copy: a line including the old ai-handbook
/// checkout's CLAUDE.md is replaced, else the include is added. Says what it did; None when it
/// was so already.
pub fn include_handbook(home: &Path) -> anyhow::Result<Option<String>> {
    let path = home.join(".claude/CLAUDE.md");
    let line = format!("@{}", crate::cli::handbook_dir().join("CLAUDE.md").display());
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let Some(new) = with_handbook_include(&text, &line) else { return Ok(None) };
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&path, &new)?;
    Ok(Some(if text.contains("ai-handbook/CLAUDE.md") {
        "~/.claude/CLAUDE.md includes the handbook from AgentFlow's copy now, instead of ~/ai-handbook".to_owned()
    } else {
        "~/.claude/CLAUDE.md includes the handbook now".to_owned()
    }))
}

/// The text with the handbook's include in it, or None when it has it already.
fn with_handbook_include(text: &str, line: &str) -> Option<String> {
    if text.lines().any(|l| l.trim() == line) {
        return None;
    }
    let mut replaced = false;
    let mut out: Vec<String> = text
        .lines()
        .map(|l| {
            let t = l.trim();
            if !replaced && t.starts_with('@') && t.ends_with("ai-handbook/CLAUDE.md") {
                replaced = true;
                line.to_owned()
            } else {
                l.to_owned()
            }
        })
        .collect();
    if !replaced {
        if out.last().is_some_and(|l| !l.trim().is_empty()) {
            out.push(String::new());
        }
        out.push(line.to_owned());
    }
    Some(out.join("\n") + "\n")
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refresh_keeps_the_session_name() {
        let hooks = |cmd: &str| {
            json!({"hooks": {"SessionStart": [{"hooks": [
                {"type": "command", "command": "/x/flow hello --doc --token-file t"},
                {"type": "command", "command": cmd}]}]}})
        };
        let name = |cmd: &str| session_name_in(&hooks(cmd));
        assert_eq!(name("/x/flow hello --no-doc --session Imaginarium --token-file t").as_deref(), Some("Imaginarium"));
        assert_eq!(name("/x/flow hello --no-doc --session 'Cold Trace' --token-file t").as_deref(), Some("Cold Trace"));
        assert_eq!(name(r"/x/flow hello --no-doc --session 'it'\''s' --token-file t").as_deref(), Some("it's"));
        assert_eq!(session_name_in(&json!({})), None);
    }

    #[test]
    fn the_handbook_include_replaces_the_old_one_or_is_added_once() {
        let line = "@/home/u/.local/share/agentflow/handbook/CLAUDE.md";
        let old = "@/home/u/ai-handbook/CLAUDE.md\n";
        assert_eq!(with_handbook_include(old, line).unwrap(), format!("{line}\n"));
        let other = "# Mine\n\nBe brief.\n";
        assert_eq!(with_handbook_include(other, line).unwrap(), format!("# Mine\n\nBe brief.\n\n{line}\n"));
        assert_eq!(with_handbook_include(&format!("{line}\n"), line), None, "once");
        assert_eq!(with_handbook_include("", line).unwrap(), format!("{line}\n"));
    }
}
