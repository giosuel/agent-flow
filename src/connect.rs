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
    let user = std::env::var("USER").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| {
        rustix::process::getuid().as_raw().to_string()
    });
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
    let asked = client.post_json("/api/approvals", &json!({"action": action, "args": args, "machine": machine_name()}))?;
    let code = asked["code"].as_str().context("no code in the answer")?.to_owned();
    let url = asked["url"].as_str().context("no link in the answer")?.to_owned();
    let poll = asked["poll"].as_str().context("no poll secret in the answer")?.to_owned();
    let wait = Duration::from_secs(asked["expires_in"].as_u64().unwrap_or(600) + 5);
    let opened = open_browser(&url);
    eprintln!("Approve this in your browser{}:", if opened { " (opened)" } else { "" });
    eprintln!("  {url}");
    eprintln!("The page shows the code {code}; approve only if it matches.");
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
            "pending" | "expired" => bail!("nobody approved it within {} minutes; run it again", crate::approvals::MINUTES),
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
    let mine = env!("APP_VERSION");
    println!("flow     {mine}");
    let url = match server_url(config_path, args) {
        Ok(u) => u,
        Err(e) => {
            println!("server   {e:#}");
            return Ok(());
        }
    };
    println!("server   {url}");
    let tf = args.token_file.clone().unwrap_or_else(token_file);
    println!("token    {}", tf.display());
    let client = match Client::new(config_path, args) {
        Ok(c) => c,
        Err(e) => {
            println!("         {e:#}");
            return Ok(());
        }
    };
    match client.get("/api/status") {
        Ok(s) => {
            let theirs = s["version"].as_str().unwrap_or("?");
            println!("version  {theirs}{}", if theirs == mine { "" } else { "  (differs from flow's: run flow update)" });
            println!(
                "as       {} ({})",
                s["who"]["name"].as_str().unwrap_or("?"),
                s["who"]["kind"].as_str().unwrap_or("?")
            );
        }
        Err(e) => println!("         unreachable: {e:#}"),
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
pub fn token_create(config_path: &Path, args: &ClientArgs, kind: &str, project: Option<&str>, name: Option<&str>, file: Option<&Path>) -> anyhow::Result<()> {
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
            println!("Token {} ({}) written to {} (only you can read it).", result["id"], result["name"].as_str().unwrap_or(""), f.display());
        }
        None => {
            println!("{token}");
            eprintln!("Token {} ({}): shown once; AgentFlow keeps only its hash.", result["id"], result["name"].as_str().unwrap_or(""));
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
        .and_then(|(a, b)| Some((a.trim().trim_start_matches('#').parse::<i64>().ok()?, b.trim().trim_start_matches('#').parse::<i64>().ok()?)))
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

/// `sudo flow server add-passkey`: asks the server on this machine, over its local socket that
/// only root and the server's user may use, for a one-time link that adds a passkey.
pub fn add_passkey(socket: Option<&Path>) -> anyhow::Result<()> {
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
        writeln!(stream, "{}", json!({"request": "add_passkey"}))?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line)?;
        let v: Value = serde_json::from_str(&line).context("a broken answer from the server")?;
        if let Some(e) = v["error"].as_str() {
            bail!("{e}");
        }
        println!("Open this within 15 minutes to add a passkey (it works once):");
        println!("  {}", v["url"].as_str().unwrap_or(""));
        return Ok(());
    }
    if tried.is_empty() {
        bail!("no AgentFlow server is running on this machine (no admin socket found)");
    }
    bail!("couldn't reach the server's socket: {} (run it with sudo)", tried.join("; "))
}
