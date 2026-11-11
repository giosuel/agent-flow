//! The agents' hands: everything AgentFlow does in the agents' world, as the agents' Unix user.
//!
//! That is: running the owner's buttons (git, gh, build commands in the checkouts), `gh` itself,
//! reading and committing guides in the handbook checkout, writing token files, setting up a
//! new project's folder, reading Claude Code's transcripts, and finding which folder an agent
//! process works in.
//!
//! On a single-user install (the server runs as the agents' user) a job simply runs in the
//! server. On a system install the server runs as its own user, which the agents can't read
//! (the owner, 2026-11-11, agreeing to "AgentFlow as its own Unix user"), and sends each job to
//! `agentflow hands`, a small process running as the agents' user, over a Unix socket that only
//! the server's user may use (checked with SO_PEERCRED). The server then needs no access to the
//! agents' home folder at all, and no GitHub login of its own.
//!
//! A forged helper (an agent replacing the process) could see the jobs and fake their results;
//! it gains nothing it doesn't have already as that user, and the server's database, tokens and
//! sessions stay out of its reach.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

/// One thing to do as the agents' user.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "job", rename_all = "snake_case")]
pub enum Job {
    /// A program with its arguments (no shell unless argv says `bash -c`).
    Exec {
        argv: Vec<String>,
        cwd: Option<PathBuf>,
        timeout_s: u64,
    },
    GuideList {
        dir: PathBuf,
    },
    GuideRead {
        dir: PathBuf,
        name: String,
    },
    GuideSave {
        dir: PathBuf,
        author: String,
        name: String,
        version: String,
        content: String,
        what: String,
    },
    GuideUnpushed {
        dir: PathBuf,
    },
    WriteToken {
        path: PathBuf,
        secret: String,
    },
    TokenExists {
        path: PathBuf,
    },
    OnboardCheck {
        project: crate::onboard::NewProject,
        taken: crate::onboard::Taken,
    },
    OnboardFiles {
        project: crate::onboard::NewProject,
        folder: PathBuf,
        secret: String,
        token_dir: PathBuf,
        claude_projects: PathBuf,
        public_url: String,
    },
    Transcripts {
        root: PathBuf,
    },
    ReadTranscript {
        root: PathBuf,
        path: PathBuf,
        offset: u64,
        first_at: Option<i64>,
    },
    /// The working folders of these processes (those of the agents' user).
    Cwds {
        pids: Vec<i32>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecOut {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

impl ExecOut {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// A job's answer on the wire: its value, or the error's text (and a code a caller can match).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Reply {
    Ok(serde_json::Value),
    Err { message: String, code: Option<String> },
}

/// An error that carries a code across the socket, e.g. a guide save's "stale".
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct Coded {
    pub code: String,
    pub message: String,
}

static SOCKET: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Where the helper listens; None runs jobs in this process. Set once at start from the config.
pub fn init(socket: Option<PathBuf>) {
    let _ = SOCKET.set(socket);
}

fn socket() -> Option<&'static PathBuf> {
    SOCKET.get().and_then(|s| s.as_ref())
}

/// Runs a job: here, or through the helper. Blocking; see [`run_async`].
pub fn run<T: serde::de::DeserializeOwned>(job: Job) -> anyhow::Result<T> {
    let value = match socket() {
        None => job.execute()?,
        Some(path) => send(path, &job)?,
    };
    Ok(serde_json::from_value(value)?)
}

pub async fn run_async<T: serde::de::DeserializeOwned + Send + 'static>(job: Job) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(move || run(job)).await?
}

/// The helper's answer can be big (a round of transcript), but not unbounded.
const MAX_REPLY: u64 = 256 * 1024 * 1024;

fn send(path: &Path, job: &Job) -> anyhow::Result<serde_json::Value> {
    let mut stream = UnixStream::connect(path)
        .with_context(|| format!("the agents' helper isn't running ({}): start agentflow-hands", path.display()))?;
    let wait = match job {
        Job::Exec { timeout_s, .. } => Duration::from_secs(timeout_s + 30),
        _ => Duration::from_secs(300),
    };
    stream.set_read_timeout(Some(wait))?;
    serde_json::to_writer(&mut stream, job)?;
    stream.write_all(b"\n")?;
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut line = Vec::new();
    BufReader::new(std::io::Read::take(&stream, MAX_REPLY)).read_until(b'\n', &mut line)?;
    match serde_json::from_slice::<Reply>(&line).context("a broken answer from the agents' helper")? {
        Reply::Ok(v) => Ok(v),
        Reply::Err { message, code: Some(code) } => Err(Coded { code, message }.into()),
        Reply::Err { message, code: None } => bail!("{message}"),
    }
}

fn to_value<T: Serialize>(v: T) -> anyhow::Result<serde_json::Value> {
    Ok(serde_json::to_value(v)?)
}

impl Job {
    /// Does it, as whoever runs this process.
    pub fn execute(self) -> anyhow::Result<serde_json::Value> {
        match self {
            Job::Exec { argv, cwd, timeout_s } => to_value(exec(&argv, cwd.as_deref(), timeout_s)?),
            Job::GuideList { dir } => to_value(crate::guides::list_here(&dir)?),
            Job::GuideRead { dir, name } => to_value(crate::guides::read_here(&dir, &name)?),
            Job::GuideSave { dir, author, name, version, content, what } => {
                let repo = crate::guides::Repo { dir, author };
                match repo.save_here(&name, &version, &content, &what) {
                    Ok(commit) => to_value(commit),
                    Err(crate::guides::SaveError::Other(e)) => Err(e),
                    Err(e) => Err(Coded { code: e.code().into(), message: e.to_string() }.into()),
                }
            }
            Job::GuideUnpushed { dir } => to_value(crate::guides::Repo { dir, author: String::new() }.unpushed_here()),
            Job::WriteToken { path, secret } => to_value(crate::tokens::write_token_file_here(&path, &secret)?),
            Job::TokenExists { path } => to_value(path.exists()),
            Job::OnboardCheck { mut project, taken } => {
                let folder = crate::onboard::check(&mut project, &taken)?;
                to_value((project, folder))
            }
            Job::OnboardFiles { project, folder, secret, token_dir, claude_projects, public_url } => to_value(
                crate::onboard::create_files(&project, &folder, &secret, &token_dir, &claude_projects, &public_url)?,
            ),
            Job::Transcripts { root } => to_value(crate::usage::transcripts(&root)),
            Job::ReadTranscript { root, path, offset, first_at } => {
                match crate::usage::read_new(&root, &path, offset, first_at) {
                    Ok(read) => to_value(read),
                    Err(e)
                        if e.downcast_ref::<std::io::Error>()
                            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
                    {
                        Err(Coded { code: "gone".into(), message: format!("{}: {e}", path.display()) }.into())
                    }
                    Err(e) => Err(e),
                }
            }
            Job::Cwds { pids } => to_value(
                pids.into_iter()
                    .filter_map(|pid| std::fs::read_link(format!("/proc/{pid}/cwd")).ok().map(|cwd| (pid, cwd)))
                    .collect::<Vec<_>>(),
            ),
        }
    }
}

/// A command with a time limit; its output is cut at 1 MB each.
fn exec(argv: &[String], cwd: Option<&Path>, timeout_s: u64) -> anyhow::Result<ExecOut> {
    use std::io::Read;
    let (program, args) = argv.split_first().context("an empty command")?;
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().with_context(|| format!("couldn't start {program}"))?;
    let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let read_out = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = (&mut out).take(1 << 20).read_to_end(&mut b);
        let _ = std::io::copy(&mut out, &mut std::io::sink());
        b
    });
    let read_err = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = (&mut err).take(1 << 20).read_to_end(&mut b);
        let _ = std::io::copy(&mut err, &mut std::io::sink());
        b
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_s);
    let mut timed_out = false;
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break Some(s);
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            timed_out = true;
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = String::from_utf8_lossy(&read_out.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&read_err.join().unwrap_or_default()).into_owned();
    Ok(ExecOut { code: status.and_then(|s| s.code()), stdout, stderr, timed_out })
}

// ---------------------------------------------------------------------------------------------
// The helper process: `agentflow hands`

/// Serves jobs on `socket` to the user `allow_uid` only, until killed.
pub fn serve(socket: &Path, allow_uid: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket).with_context(|| format!("listening on {}", socket.display()))?;
    // The folder (made by setup) lets only the server's group in; the socket itself is ours.
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o660))?;
    eprintln!("agentflow hands: serving jobs on {} for uid {allow_uid}", socket.display());
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        std::thread::spawn(move || {
            if let Err(e) = handle(stream, allow_uid) {
                eprintln!("agentflow hands: {e:#}");
            }
        });
    }
    Ok(())
}

/// Requests are small; a guide's text is the largest.
const MAX_REQUEST: u64 = 4 * 1024 * 1024;

fn handle(mut stream: UnixStream, allow_uid: u32) -> anyhow::Result<()> {
    let peer = rustix::net::sockopt::socket_peercred(&stream)?;
    if peer.uid.as_raw() != allow_uid {
        bail!("refused a connection from uid {} (pid {})", peer.uid.as_raw(), peer.pid.as_raw_nonzero());
    }
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut line = Vec::new();
    BufReader::new(std::io::Read::take(&stream, MAX_REQUEST)).read_until(b'\n', &mut line)?;
    let reply = match serde_json::from_slice::<Job>(&line) {
        Err(e) => Reply::Err { message: format!("not a job: {e}"), code: None },
        Ok(job) => match job.execute() {
            Ok(v) => Reply::Ok(v),
            Err(e) => match e.downcast::<Coded>() {
                Ok(c) => Reply::Err { message: c.message, code: Some(c.code) },
                Err(e) => Reply::Err { message: format!("{e:#}"), code: None },
            },
        },
    };
    serde_json::to_writer(&mut stream, &reply)?;
    stream.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jobs_over_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("hands.sock");
        let me = rustix::process::getuid().as_raw();
        let s = sock.clone();
        std::thread::spawn(move || serve(&s, me));
        while !sock.exists() {
            std::thread::sleep(Duration::from_millis(5));
        }
        let out: ExecOut = serde_json::from_value(
            send(
                &sock,
                &Job::Exec {
                    argv: vec!["sh".into(), "-c".into(), "echo hi; echo err >&2; exit 3".into()],
                    cwd: None,
                    timeout_s: 5,
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!((out.code, out.stdout.as_str(), out.stderr.as_str()), (Some(3), "hi\n", "err\n"));
        let slow: ExecOut = serde_json::from_value(
            send(&sock, &Job::Exec { argv: vec!["sleep".into(), "5".into()], cwd: None, timeout_s: 1 }).unwrap(),
        )
        .unwrap();
        assert!(slow.timed_out && slow.code.is_none());
        // Errors keep their code across the socket.
        std::fs::write(dir.path().join("A.md"), "# A\n").unwrap();
        let e = send(
            &sock,
            &Job::GuideSave {
                dir: dir.path().into(),
                author: "o <o@x>".into(),
                name: "A.md".into(),
                version: "old".into(),
                content: "x".into(),
                what: "t".into(),
            },
        )
        .unwrap_err();
        assert_eq!(e.downcast_ref::<Coded>().map(|c| c.code.as_str()), Some("stale"));
        let g: crate::guides::Guide = serde_json::from_value(
            send(&sock, &Job::GuideRead { dir: dir.path().into(), name: "A.md".into() }).unwrap(),
        )
        .unwrap();
        assert_eq!(g.text, "# A\n");
    }

    #[test]
    fn other_users_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("hands.sock");
        let s = sock.clone();
        let someone_else = rustix::process::getuid().as_raw() + 1;
        std::thread::spawn(move || serve(&s, someone_else));
        while !sock.exists() {
            std::thread::sleep(Duration::from_millis(5));
        }
        // The connection is dropped without an answer (reset, or an empty reply).
        assert!(send(&sock, &Job::GuideList { dir: dir.path().into() }).is_err());
    }
}
