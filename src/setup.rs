//! `agentflow setup`: makes AgentFlow a system service running as its own user, so the agents
//! (another user) can't read its database, tokens or sessions. The owner, 2026-11-11, agreeing to:
//! the package decides the user, and one setup command configures the rest.
//!
//! Run once with sudo after installing (the deb and AUR packages bring the user, the units and
//! the binary; from a plain binary it writes those too). Again later to see what's set; it never
//! overwrites a config. `--dry-run` prints every step without doing it.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

const DATA: &str = "/var/lib/agentflow";
const SOCKET_DIR: &str = "/run/agentflow-hands";
const UNIT: &str = include_str!("../packaging/system/agentflow.service");
const HANDS_UNIT: &str = include_str!("../packaging/system/agentflow-hands.service");
const SYSUSERS: &str = include_str!("../packaging/system/agentflow.sysusers");

#[derive(clap::Args, Debug)]
pub struct SetupArgs {
    /// The address people and agents use, e.g. https://flow.giosuel.io (behind your reverse proxy).
    #[arg(long)]
    pub public_url: String,
    /// Where the server listens; your reverse proxy forwards to it.
    #[arg(long, default_value = "127.0.0.1:8790")]
    pub listen: String,
    /// The Unix user the Claude agents run as (default: whoever ran sudo).
    #[arg(long)]
    pub agents_user: Option<String>,
    /// A single-user install's data folder to bring along (database, push keys, builds), e.g.
    /// /home/viber/.local/state/agentflow. Stop that instance first.
    #[arg(long)]
    pub import: Option<PathBuf>,
    /// Print the steps instead of doing them.
    #[arg(long)]
    pub dry_run: bool,
    /// Write the files below this folder and run nothing (tests).
    #[arg(long, hide = true)]
    pub root: Option<PathBuf>,
}

#[derive(Debug, PartialEq)]
enum Step {
    Write { path: PathBuf, text: String, mode: u32, why: String },
    Run { argv: Vec<String>, why: String },
    Import { from: PathBuf },
}

/// A user from /etc/passwd: (uid, home).
fn user(name: &str, passwd: &str) -> anyhow::Result<(u32, PathBuf)> {
    for line in passwd.lines() {
        let f: Vec<&str> = line.split(':').collect();
        if f.len() >= 7 && f[0] == name {
            return Ok((f[2].parse()?, f[5].into()));
        }
    }
    bail!("no user {name:?} on this machine")
}

fn run(argv: &[&str], why: &str) -> Step {
    Step::Run { argv: argv.iter().map(|s| s.to_string()).collect(), why: why.into() }
}

fn plan(
    a: &SetupArgs,
    agents: &str,
    agents_home: &Path,
    bin: &Path,
    exists: &dyn Fn(&str) -> bool,
) -> anyhow::Result<Vec<Step>> {
    let listen: std::net::SocketAddr = a.listen.parse().context("--listen must be an address like 127.0.0.1:8790")?;
    if listen.ip().is_unspecified() {
        bail!("--listen {listen} would be reachable from everywhere; use 127.0.0.1 behind your reverse proxy");
    }
    if !(a.public_url.starts_with("https://") || a.public_url.starts_with("http://")) {
        bail!("--public-url must start with https:// (or http:// on a private network)");
    }
    let mut steps = Vec::new();
    if !exists("/usr/lib/sysusers.d/agentflow.conf") {
        steps.push(Step::Write {
            path: "/etc/sysusers.d/agentflow.conf".into(),
            text: SYSUSERS.into(),
            mode: 0o644,
            why: "AgentFlow's own user".into(),
        });
    }
    steps.push(run(&["systemd-sysusers"], "create the user agentflow"));
    if exists("/etc/agentflow/config.toml") {
        eprintln!("/etc/agentflow/config.toml exists: kept as it is.");
    } else {
        steps.push(Step::Write {
            path: "/etc/agentflow/config.toml".into(),
            text: format!(
                "# Written by `agentflow setup`. AgentFlow runs as the user agentflow, the agents as {agents};\n\
                 # `~/` in paths is the agents' home. Readable by all: it holds no secrets.\n\
                 listen = \"{listen}\"\npublic_url = \"{url}\"\ndata_dir = \"{DATA}\"\n\
                 agents_home = \"{home}\"\nhands_socket = \"{SOCKET_DIR}/hands.sock\"\n",
                url = a.public_url.trim_end_matches('/'),
                home = agents_home.display(),
            ),
            mode: 0o644,
            why: "the server's settings".into(),
        });
    }
    // The helper (the agents' user) makes its socket here; the setgid bit gives the socket the
    // group agentflow, so the server can connect and no one else can.
    steps.push(Step::Write {
        path: "/etc/tmpfiles.d/agentflow-hands.conf".into(),
        text: format!("d {SOCKET_DIR} 2750 {agents} agentflow -\n"),
        mode: 0o644,
        why: "the helper's socket folder, at every boot".into(),
    });
    steps.push(run(&["systemd-tmpfiles", "--create", "/etc/tmpfiles.d/agentflow-hands.conf"], "make it now"));
    let bin = bin.display().to_string();
    if !exists("/usr/lib/systemd/system/agentflow.service") {
        steps.push(Step::Write {
            path: "/etc/systemd/system/agentflow.service".into(),
            text: UNIT.replace("@BIN@", &bin),
            mode: 0o644,
            why: "the server's service".into(),
        });
    }
    if !exists("/usr/lib/systemd/user/agentflow-hands.service") {
        steps.push(Step::Write {
            path: "/etc/systemd/user/agentflow-hands.service".into(),
            text: HANDS_UNIT.replace("@BIN@", &bin),
            mode: 0o644,
            why: "the helper's service, for the agents' user".into(),
        });
    }
    if let Some(from) = &a.import {
        if exists(&format!("{DATA}/flow.db")) {
            bail!("{DATA}/flow.db exists already; not importing over it");
        }
        steps.push(run(&["install", "-d", "-m", "700", "-o", "agentflow", "-g", "agentflow", DATA], "its data folder"));
        steps.push(Step::Import { from: from.clone() });
        steps.push(run(&["chown", "-R", "agentflow:agentflow", DATA], "the imported data is AgentFlow's"));
    }
    steps.push(run(&["systemctl", "daemon-reload"], "load the units"));
    steps.push(run(&["loginctl", "enable-linger", agents], "the helper runs without a login session"));
    steps.push(run(&["systemctl", "enable", "--now", "agentflow"], "start the server"));
    Ok(steps)
}

/// The service runs this binary as agentflow: anyone who can replace it becomes AgentFlow. So it
/// (and its folder) must belong to root and be writable by no one else, e.g. /usr/local/bin.
fn check_binary(bin: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    for p in [bin, bin.parent().unwrap_or(Path::new("/"))] {
        let m = std::fs::metadata(p)?;
        if m.uid() != 0 || m.mode() & 0o022 != 0 {
            bail!(
                "{} can be changed by someone other than root, and the service would run it. Install it first:\n  sudo install -m 755 {} /usr/local/bin/agentflow\nthen run /usr/local/bin/agentflow setup …",
                p.display(),
                bin.display()
            );
        }
    }
    Ok(())
}

/// Copies a single-user data folder: the database as a consistent copy, everything else as is.
fn import(from: &Path, to: &Path) -> anyhow::Result<()> {
    let db = from.join("flow.db");
    if !db.exists() {
        bail!("{} has no flow.db", from.display());
    }
    std::fs::create_dir_all(to)?;
    let src = rusqlite::Connection::open(&db)?;
    src.execute("VACUUM INTO ?1", [to.join("flow.db").to_string_lossy()])?;
    fn copy(from: &Path, to: &Path) -> anyhow::Result<()> {
        for e in std::fs::read_dir(from)? {
            let e = e?;
            let name = e.file_name();
            if name.to_string_lossy().starts_with("flow.db") {
                continue;
            }
            let t = e.file_type()?;
            if t.is_dir() {
                std::fs::create_dir_all(to.join(&name))?;
                copy(&e.path(), &to.join(&name))?;
            } else if t.is_file() {
                std::fs::copy(e.path(), to.join(&name))?;
            }
        }
        Ok(())
    }
    copy(from, to)
}

pub fn setup(a: SetupArgs) -> anyhow::Result<()> {
    let root = a.root.clone();
    let under = |p: &str| match &root {
        Some(r) => r.join(p.trim_start_matches('/')),
        None => PathBuf::from(p),
    };
    if root.is_none() && !a.dry_run && !rustix::process::geteuid().is_root() {
        bail!("setup changes the system; run it with sudo (or add --dry-run to see the steps)");
    }
    let agents = match a.agents_user.clone().or_else(|| std::env::var("SUDO_USER").ok()) {
        Some(u) if u != "root" => u,
        _ => bail!("which user do the agents run as? Give --agents-user"),
    };
    let passwd = std::fs::read_to_string(under("/etc/passwd")).unwrap_or_default();
    let passwd = if passwd.is_empty() { std::fs::read_to_string("/etc/passwd")? } else { passwd };
    let (_, agents_home) = user(&agents, &passwd)?;
    if root.is_none() && !a.dry_run && std::net::TcpListener::bind(&a.listen).is_err() {
        bail!(
            "something listens on {} already (the single-user AgentFlow?). Stop it first, as {agents}: systemctl --user disable --now agentflow",
            a.listen
        );
    }
    let bin = std::env::current_exe()?;
    if root.is_none() && !under("/usr/lib/systemd/system/agentflow.service").exists() {
        check_binary(&bin)?;
    }
    let steps = plan(&a, &agents, &agents_home, &bin, &|p| under(p).exists())?;
    for step in &steps {
        match step {
            Step::Write { path, text, mode, why } => {
                println!("write {} ({why})", path.display());
                if a.dry_run {
                    continue;
                }
                use std::os::unix::fs::PermissionsExt;
                let path = under(&path.to_string_lossy());
                std::fs::create_dir_all(path.parent().context("no folder")?)?;
                std::fs::write(&path, text)?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(*mode))?;
            }
            Step::Run { argv, why } => {
                println!("run   {} ({why})", argv.join(" "));
                if a.dry_run || root.is_some() {
                    continue;
                }
                let ok = std::process::Command::new(&argv[0]).args(&argv[1..]).status()?.success();
                if !ok {
                    bail!("{} failed; fix that and run setup again", argv.join(" "));
                }
            }
            Step::Import { from } => {
                println!("copy  {} → {DATA} (the database as a consistent copy)", from.display());
                if !a.dry_run {
                    import(from, &under(DATA))?;
                }
            }
        }
    }
    println!(
        "\nNow, as {agents} (fish works too):\n  systemctl --user daemon-reload; systemctl --user enable --now agentflow-hands\n\
         Then add your first passkey with the link it prints (valid 15 minutes):\n  sudo flow server add-passkey"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(root: &Path) -> SetupArgs {
        SetupArgs {
            public_url: "https://flow.example.org/".into(),
            listen: "127.0.0.1:8790".into(),
            agents_user: Some("viber".into()),
            import: None,
            dry_run: false,
            root: Some(root.to_owned()),
        }
    }

    #[test]
    fn writes_the_system_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("etc")).unwrap();
        std::fs::write(
            root.path().join("etc/passwd"),
            "root:x:0:0::/root:/bin/sh\nviber:x:1000:1000::/home/viber:/usr/bin/fish\n",
        )
        .unwrap();
        setup(args(root.path())).unwrap();
        let read = |p: &str| std::fs::read_to_string(root.path().join(p)).unwrap();
        let config: crate::config::Config = toml::from_str(&read("etc/agentflow/config.toml")).unwrap();
        assert_eq!(config.public_url, "https://flow.example.org");
        assert_eq!((config.agents_home.as_str(), config.data_dir.as_str()), ("/home/viber", DATA));
        assert_eq!(config.hands_socket, "/run/agentflow-hands/hands.sock");
        assert_eq!(read("etc/tmpfiles.d/agentflow-hands.conf"), "d /run/agentflow-hands 2750 viber agentflow -\n");
        let unit = read("etc/systemd/system/agentflow.service");
        assert!(unit.contains("User=agentflow") && unit.contains("ProtectHome=yes") && !unit.contains("@BIN@"));
        assert!(read("etc/systemd/user/agentflow-hands.service").contains(" hands"));
        // A second run keeps the config.
        std::fs::write(root.path().join("etc/agentflow/config.toml"), "listen = \"127.0.0.1:9\"\n").unwrap();
        setup(args(root.path())).unwrap();
        assert_eq!(read("etc/agentflow/config.toml"), "listen = \"127.0.0.1:9\"\n");
    }

    #[test]
    fn refuses_a_public_listen_and_unknown_users() {
        let root = tempfile::tempdir().unwrap();
        let mut a = args(root.path());
        a.listen = "0.0.0.0:8790".into();
        assert!(plan(&a, "viber", Path::new("/home/viber"), Path::new("/usr/bin/agentflow"), &|_| false).is_err());
        assert!(user("nobody-here", "viber:x:1000:1000::/home/viber:/bin/sh\n").is_err());
    }

    #[test]
    fn imports_a_single_user_folder() {
        let from = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(&from.path().join("flow.db")).unwrap();
        db.call_blocking(|c| crate::projects::create(c, "vinculum", "Vinculum", "t")).unwrap();
        std::fs::write(from.path().join("vapid.key"), "k").unwrap();
        std::fs::create_dir_all(from.path().join("builds/vinculum")).unwrap();
        std::fs::write(from.path().join("builds/vinculum/b.tar.zst"), "b").unwrap();
        let to = tempfile::tempdir().unwrap();
        import(from.path(), to.path()).unwrap();
        let copy = rusqlite::Connection::open(to.path().join("flow.db")).unwrap();
        let n: i64 = copy.query_row("SELECT count(*) FROM projects", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        assert_eq!(std::fs::read_to_string(to.path().join("vapid.key")).unwrap(), "k");
        assert!(to.path().join("builds/vinculum/b.tar.zst").exists());
    }
}
