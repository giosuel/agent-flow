//! `flow builder add | list | edit | remove` (the owner, 2026-11-13): build machines are added
//! from the machine itself, with the owner's approval in the browser. By default the builder gets
//! a user of its own ("Do you want to add a new user for the builder? [Y/n]"), so projects' build
//! scripts and tests never see the owner's files; its service keeps running when nobody is logged
//! in (linger). One builder per user.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde_json::json;

use crate::cli::{Client, ClientArgs};

const UNIT: &str = "agentflow-builder.service";

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

fn yes(question: &str, default_yes: bool) -> anyhow::Result<bool> {
    let a = ask(&format!("{question} {}", if default_yes { "[Y/n]" } else { "[y/N]" }), "")?;
    Ok(match a.to_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        _ => false,
    })
}

/// A Unix user name we create: lowercase letters, digits, `-` and `_`.
fn check_user(u: &str) -> anyhow::Result<()> {
    let ok = !u.is_empty()
        && u.len() <= 32
        && u.starts_with(|c: char| c.is_ascii_lowercase())
        && u.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !ok {
        bail!("a user name is lowercase letters, digits, - and _, starting with a letter");
    }
    Ok(())
}

/// A user's home, from the system's user database.
fn home_of(user: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("getent").args(["passwd", user]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().split(':').nth(5).map(PathBuf::from)
}

fn me() -> String {
    std::env::var("USER").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| rustix::process::getuid().as_raw().to_string())
}

/// Shows a command, then runs it (sudo asks for the password itself).
fn run_shown(argv: &[&str], input: Option<&[u8]>) -> anyhow::Result<()> {
    eprintln!("  $ {}", argv.join(" "));
    let mut cmd = std::process::Command::new(argv[0]);
    cmd.args(&argv[1..]);
    if input.is_some() {
        cmd.stdin(std::process::Stdio::piped());
    }
    let mut child = cmd.spawn().with_context(|| format!("running {}", argv[0]))?;
    if let Some(data) = input {
        child.stdin.take().context("stdin")?.write_all(data)?;
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("{} failed", argv.join(" "));
    }
    Ok(())
}

/// The machine's threads and memory, for the starting limits.
fn machine() -> (u64, f64) {
    let threads = std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(1);
    let mem = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|t| t.lines().find(|l| l.starts_with("MemTotal:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<f64>().ok()))
        .map(|kb| kb / 1024.0 / 1024.0)
        .unwrap_or(0.0);
    (threads, (mem * 10.0).round() / 10.0)
}

/// The agentflow-builder program to run: a package's, else the one next to this flow.
fn builder_program() -> anyhow::Result<PathBuf> {
    let packaged = PathBuf::from("/usr/bin/agentflow-builder");
    if packaged.is_file() {
        return Ok(packaged);
    }
    let me = std::env::current_exe()?;
    let next = me.parent().map(|d| d.join("agentflow-builder")).filter(|p| p.is_file());
    next.context("agentflow-builder isn't installed next to flow (install it with the install command or the package)")
}

fn unit_text(program: &Path) -> String {
    format!(
        "# AgentFlow's builder (written by flow builder add).\n[Unit]\nDescription=AgentFlow builder\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nExecStart={}\nRestart=always\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
        program.display()
    )
}

fn packaged_unit() -> bool {
    Path::new("/usr/lib/systemd/user").join(UNIT).is_file()
}

fn linger_on(user: &str) -> bool {
    std::process::Command::new("loginctl")
        .args(["show-user", user, "-p", "Linger", "--value"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "yes")
        .unwrap_or(false)
}

pub struct AddArgs {
    pub name: Option<String>,
    pub user: Option<String>,
    pub current_user: bool,
}

/// `flow builder add`.
pub fn add(config_path: &Path, args: &ClientArgs, a: AddArgs) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    let client = Client::new(config_path, args).context("adding a builder needs a connected machine: flow connect <server> first")?;
    let interactive = std::io::stdin().is_terminal();
    let host = crate::machine::hostname();
    // Its own user, unless asked otherwise (the owner, 2026-11-13: new user by default).
    let new_user = if a.current_user {
        false
    } else if a.user.is_some() || !interactive {
        a.user.is_some()
    } else {
        yes("Do you want to add a new user for the builder?", true)?
    };
    let user = if new_user {
        let u = match a.user {
            Some(u) => u,
            None => ask("User name", "agentflow-builder")?,
        };
        check_user(&u)?;
        u
    } else {
        me()
    };
    let name = match a.name {
        Some(n) => n,
        None if interactive => ask("Builder name", &host)?,
        None => host.clone(),
    };
    // One builder per user.
    let home = if new_user { home_of(&user).unwrap_or_else(|| PathBuf::from(format!("/home/{user}"))) } else { crate::util::home_dir() };
    let conf = home.join(".config/agentflow/builder.toml");
    let exists = if new_user && user != me() {
        std::process::Command::new("sudo").args(["-n", "test", "-e"]).arg(&conf).status().map(|s| s.success()).unwrap_or(false)
    } else {
        conf.exists()
    };
    if exists {
        let current = std::fs::read_to_string(&conf).ok().and_then(|t| t.parse::<toml::Table>().ok()).and_then(|t| t.get("name").and_then(|v| v.as_str()).map(str::to_owned));
        bail!(
            "A builder is already registered for this user{}. Change it with `flow builder edit`, or remove it with `flow builder remove` first.",
            current.map(|c| format!(" ({c})")).unwrap_or_default()
        );
    }
    let program = builder_program()?;
    let (threads, memory_gb) = machine();
    let result = crate::connect::approve(
        &client,
        "builder_create",
        json!({"name": name, "threads": threads, "memory_gb": memory_gb, "user": user}),
    )?;
    let token = result["token"].as_str().context("the server sent no token")?.to_owned();
    eprintln!("  ✓ approved: builder {name}");
    let toml_text = format!("url = {}\nname = {}\n", toml::Value::String(client.base.clone()), toml::Value::String(name.clone()));
    if new_user {
        set_up_other_user(&user, &program, &toml_text, &token)?;
    } else {
        set_up_here(&program, &toml_text, &token)?;
        if !linger_on(&user) {
            eprintln!("Its service stops when you log out. To keep it running, once:");
            eprintln!("  sudo loginctl enable-linger {user}");
        }
    }
    eprintln!(
        "  ✓ limits to start with: another job while {} threads and {} GB are free; jobs pause below {} GB",
        result["min_threads"], result["min_memory_gb"], result["reserve_memory_gb"]
    );
    // Wait until it's online.
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        if let Ok(list) = client.get("/api/builders")
            && let Some(b) = list["builders"].as_array().and_then(|a| a.iter().find(|b| b["name"] == name.as_str()))
            && b["online"].as_bool().unwrap_or(false)
        {
            println!(
                "{name} is online: {} threads, {} GB{}.",
                b["threads"].as_u64().unwrap_or(threads),
                b["memory_gb"].as_f64().unwrap_or(memory_gb),
                if new_user { format!(", as {user}") } else { String::new() }
            );
            println!("Projects use it once you pick it in their Settings; agents set up their tools with flow remote --setup.");
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    bail!("{name} didn't come online within 30 s; see journalctl --user -u agentflow-builder (as {user})")
}

/// Writes a file with a mode (creating its folder).
fn write_mode(path: &Path, text: &str, mode: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("new");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
    f.write_all(text.as_bytes())?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn set_up_here(program: &Path, toml_text: &str, token: &str) -> anyhow::Result<()> {
    let home = crate::util::home_dir();
    write_mode(&home.join(".config/agentflow/builder.toml"), toml_text, 0o600)?;
    write_mode(&home.join(".config/agentflow/builder.token"), &format!("{token}\n"), 0o600)?;
    if !packaged_unit() {
        write_mode(&home.join(".config/systemd/user").join(UNIT), &unit_text(program), 0o644)?;
    }
    run_shown(&["systemctl", "--user", "daemon-reload"], None)?;
    run_shown(&["systemctl", "--user", "enable", "--now", UNIT], None)?;
    eprintln!("  ✓ config and token in ~/.config/agentflow, service {UNIT} running");
    Ok(())
}

/// A user of its own: made with sudo (every command shown first), linger on, the program, config,
/// token and service in its home, the service started as that user.
fn set_up_other_user(user: &str, program: &Path, toml_text: &str, token: &str) -> anyhow::Result<()> {
    if home_of(user).is_none() {
        run_shown(&["sudo", "useradd", "--create-home", "--shell", "/usr/sbin/nologin", user], None)?;
    } else {
        eprintln!("  ✓ the user {user} exists already");
    }
    run_shown(&["sudo", "loginctl", "enable-linger", user], None)?;
    let home = home_of(user).context("the new user has no home")?;
    let h = |p: &str| home.join(p).display().to_string();
    run_shown(&["sudo", "install", "-d", "-o", user, "-g", user, "-m", "700", &h(".config"), &h(".config/agentflow"), &h(".config/systemd"), &h(".config/systemd/user"), &h(".local"), &h(".local/bin")], None)?;
    let program_there = if program.starts_with("/usr/") {
        program.to_path_buf()
    } else {
        let to = home.join(".local/bin/agentflow-builder");
        run_shown(&["sudo", "install", "-o", user, "-g", user, "-m", "755", &program.display().to_string(), &to.display().to_string()], None)?;
        to
    };
    run_shown(&["sudo", "install", "-o", user, "-g", user, "-m", "600", "/dev/stdin", &h(".config/agentflow/builder.toml")], Some(toml_text.as_bytes()))?;
    eprintln!("  (the token goes in through stdin, so it's in no command line)");
    run_shown(&["sudo", "install", "-o", user, "-g", user, "-m", "600", "/dev/stdin", &h(".config/agentflow/builder.token")], Some(format!("{token}\n").as_bytes()))?;
    if !packaged_unit() {
        let unit = unit_text(&program_there);
        run_shown(&["sudo", "install", "-o", user, "-g", user, "-m", "644", "/dev/stdin", &h(&format!(".config/systemd/user/{UNIT}"))], Some(unit.as_bytes()))?;
    }
    let machine = format!("{user}@");
    run_shown(&["sudo", "systemctl", "--user", "-M", &machine, "daemon-reload"], None)?;
    run_shown(&["sudo", "systemctl", "--user", "-M", &machine, "enable", "--now", UNIT], None)?;
    eprintln!("  ✓ user {user} with linger, its config, token and service running");
    Ok(())
}

pub fn list(config_path: &Path, args: &ClientArgs) -> anyhow::Result<()> {
    let client = Client::new(config_path, args)?;
    let v = client.get("/api/builders")?;
    let rows = v["builders"].as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("No builders yet: flow builder add, on the machine.");
        return Ok(());
    }
    for b in rows {
        println!(
            "{:<16} {:<7} {:<8} {:>3} jobs   start at {} threads + {} GB free; pause below {} GB; folders kept {} days{}",
            b["name"].as_str().unwrap_or(""),
            b["kind"].as_str().unwrap_or(""),
            if b["online"].as_bool().unwrap_or(false) { "online" } else { "offline" },
            b["jobs"],
            b["min_threads"],
            b["min_memory_gb"],
            b["reserve_memory_gb"],
            b["keep_days"],
            b["threads"].as_u64().map(|t| format!("   ({t} threads, {} GB)", b["memory_gb"])).unwrap_or_default()
        );
    }
    Ok(())
}

/// This user's builder, from its config.
fn mine() -> Option<(String, String, String)> {
    let home = crate::util::home_dir();
    let t: toml::Table = std::fs::read_to_string(home.join(".config/agentflow/builder.toml")).ok()?.parse().ok()?;
    let token = std::fs::read_to_string(home.join(".config/agentflow/builder.token")).ok()?.trim().to_owned();
    Some((t.get("name")?.as_str()?.to_owned(), t.get("url")?.as_str()?.to_owned(), token))
}

pub struct EditArgs {
    pub name: Option<String>,
    pub rename: Option<String>,
    pub min_threads: Option<f64>,
    pub min_memory: Option<f64>,
    pub reserve_memory: Option<f64>,
    pub keep_days: Option<i64>,
}

pub fn edit(config_path: &Path, args: &ClientArgs, e: EditArgs) -> anyhow::Result<()> {
    let client = Client::new(config_path, args)?;
    let name = match e.name.or_else(|| mine().map(|m| m.0)) {
        Some(n) => n,
        None => bail!("which builder? flow builder edit <name> (flow builder list shows them)"),
    };
    let body = json!({"name": e.rename, "min_threads": e.min_threads, "min_memory_gb": e.min_memory, "reserve_memory_gb": e.reserve_memory, "keep_days": e.keep_days});
    let res = client.request_json(reqwest::Method::PATCH, &format!("/api/builders/edit/{name}"), &body)?;
    let now = res["name"].as_str().unwrap_or(&name).to_owned();
    // The builder here was renamed: its config follows.
    if now != name && mine().is_some_and(|m| m.0 == name) {
        let home = crate::util::home_dir();
        let path = home.join(".config/agentflow/builder.toml");
        let text = std::fs::read_to_string(&path)?.replace(&format!("name = \"{name}\""), &format!("name = \"{now}\""));
        write_mode(&path, &text, 0o600)?;
        let _ = std::process::Command::new("systemctl").args(["--user", "restart", UNIT]).status();
    }
    println!("Saved{}.", if now != name { format!(" ({name} is now {now})") } else { String::new() });
    Ok(())
}

pub fn remove(config_path: &Path, args: &ClientArgs, name: Option<String>) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    let here = mine();
    let local = match (&name, &here) {
        (None, Some(_)) => true,
        (Some(n), Some(m)) => n == &m.0,
        _ => false,
    };
    if local {
        let (n, url, token) = here.context("this user's builder")?;
        Client::with_token(&url, &token).delete("/api/builders/self")?;
        let _ = std::process::Command::new("systemctl").args(["--user", "disable", "--now", UNIT]).status();
        let home = crate::util::home_dir();
        for f in [".config/agentflow/builder.toml", ".config/agentflow/builder.token"] {
            let _ = std::fs::remove_file(home.join(f));
        }
        let unit = home.join(".config/systemd/user").join(UNIT);
        let _ = std::fs::remove_file(&unit);
        let delete = if std::io::stdin().is_terminal() { yes("Delete its work folders and stored files (they can be rebuilt)?", true)? } else { false };
        if delete {
            let _ = std::fs::remove_dir_all(home.join("agentflow"));
            let _ = std::fs::remove_dir_all(home.join(".local/share/agentflow-builder"));
        }
        println!("{n} is removed: its token no longer works, its service is off{}.", if delete { ", its folders are gone" } else { "" });
        return Ok(());
    }
    let Some(name) = name else { bail!("which builder? flow builder remove <name> (flow builder list shows them)") };
    let client = Client::new(config_path, args)?;
    crate::connect::approve(&client, "builder_remove", json!({"name": name}))?;
    println!("{name} is removed: its token no longer works. On that machine, its service keeps trying until you stop it.");
    Ok(())
}
