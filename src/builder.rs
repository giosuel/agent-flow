//! `agentflow-builder`: the builder's side (see `crate::builders`). It connects out to the server
//! over a WebSocket and keeps reconnecting; it runs what the server asks in the builder user's home
//! (scripts, a job's command), brings a project's work folder to a snapshot from the server's
//! content store, and sends results back over HTTPS.
//!
//! Answers go through an outbox that outlives a connection: a job keeps running when the network
//! drops, and its lines and end are sent when the builder is back (the owner, 2026-11-13).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::blobs::{Manifest, Store};
use crate::builders::{Reply, Request};
use crate::util::expand_home;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// The AgentFlow server, e.g. https://flow.example
    pub url: String,
    /// This builder's name there.
    pub name: String,
    /// Its token (default ~/.config/agentflow/builder.token).
    #[serde(default)]
    pub token_file: String,
}

impl Config {
    pub fn default_path() -> PathBuf {
        expand_home("~/.config/agentflow/builder.toml")
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {} (flow builder add writes it)", path.display()))?;
        let c: Config = toml::from_str(&text)?;
        Ok(c)
    }

    fn token(&self) -> anyhow::Result<String> {
        let path = if self.token_file.is_empty() {
            expand_home("~/.config/agentflow/builder.token")
        } else {
            expand_home(&self.token_file)
        };
        let t = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        Ok(t.trim().to_owned())
    }
}

/// Where the builder keeps files by content.
fn store() -> Store {
    Store::new(expand_home("~/.local/share/agentflow-builder/store"))
}

#[derive(Default)]
struct Outbox {
    queue: Mutex<VecDeque<Reply>>,
    wake: tokio::sync::Notify,
}

impl Outbox {
    fn push(&self, r: Reply) {
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).push_back(r);
        self.wake.notify_one();
    }
}

struct Builder {
    cfg: Config,
    token: String,
    out: Outbox,
    /// Streamed scripts that can be stopped, by request id.
    kills: Mutex<HashMap<u64, tokio::sync::oneshot::Sender<()>>>,
}

/// Runs forever: connect, serve, reconnect with a growing pause.
pub async fn run(cfg: Config) -> anyhow::Result<()> {
    let token = cfg.token()?;
    let me = Arc::new(Builder { cfg, token, out: Outbox::default(), kills: Mutex::new(HashMap::new()) });
    let mut pause = 1u64;
    loop {
        match session(&me).await {
            Ok(()) => {
                eprintln!("disconnected; connecting again");
                pause = 1;
            }
            Err(e) => eprintln!("{e:#}; trying again in {pause} s"),
        }
        tokio::time::sleep(Duration::from_secs(pause)).await;
        pause = (pause * 2).min(60);
    }
}

fn ws_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_owned()
    };
    format!("{ws}/api/builders/connect")
}

async fn session(me: &Arc<Builder>) -> anyhow::Result<()> {
    let mut req = ws_url(&me.cfg.url).into_client_request()?;
    req.headers_mut().insert("authorization", format!("Bearer {}", me.token).parse()?);
    let (socket, _) = tokio_tungstenite::connect_async(req).await.context("connecting")?;
    eprintln!("connected to {} as {}", me.cfg.url, me.cfg.name);
    let (mut sink, mut stream) = socket.split();
    let (threads, memory_gb) = machine();
    let hello = Reply::Hello { name: me.cfg.name.clone(), version: env!("APP_VERSION").into(), threads, memory_gb };
    sink.send(Message::Text(serde_json::to_string(&hello)?.into())).await?;
    loop {
        // Send everything waiting, keeping what couldn't go for the next connection.
        loop {
            let next = me.out.queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
            let Some(r) = next else { break };
            let text = serde_json::to_string(&r)?;
            if let Err(e) = sink.send(Message::Text(text.into())).await {
                me.out.queue.lock().unwrap_or_else(|e| e.into_inner()).push_front(r);
                return Err(e.into());
            }
        }
        tokio::select! {
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(t))) => match serde_json::from_str::<Request>(&t) {
                    Ok(r) => handle(me.clone(), r),
                    Err(e) => eprintln!("an odd request: {e}"),
                },
                Some(Ok(Message::Ping(p))) => sink.send(Message::Pong(p)).await?,
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(Err(e)) => return Err(e.into()),
                _ => {}
            },
            _ = me.out.wake.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(60)) => {
                sink.send(Message::Ping(Vec::new().into())).await?;
            }
        }
    }
}

/// Threads and memory, for the server's page.
fn machine() -> (u64, f64) {
    let threads = std::thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(1);
    let mem = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<f64>().ok())
        })
        .map(|kb| kb / 1024.0 / 1024.0)
        .unwrap_or(0.0);
    (threads, (mem * 10.0).round() / 10.0)
}

fn handle(me: Arc<Builder>, req: Request) {
    match req {
        Request::Kill { id } => {
            if let Some(tx) = me.kills.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) {
                let _ = tx.send(());
            }
        }
        Request::Sh { id, script, stream, timeout_s } => {
            tokio::spawn(async move {
                let reply = match sh(&me, id, &script, stream, timeout_s).await {
                    Ok(r) => r,
                    Err(e) => Reply::Failed { re: id, error: format!("{e:#}") },
                };
                me.out.push(reply);
            });
        }
        Request::Sync { id, work, manifest, bundle, commit: _ } => {
            let me2 = me.clone();
            tokio::spawn(async move {
                let got = tokio::task::spawn_blocking(move || sync(&me2, &work, &manifest, &bundle)).await;
                let reply = match got {
                    Ok(Ok(report)) => {
                        Reply::Done { re: id, code: 0, stdout: report.to_string(), stderr: String::new() }
                    }
                    Ok(Err(e)) => Reply::Done { re: id, code: 1, stdout: String::new(), stderr: format!("{e:#}") },
                    Err(e) => Reply::Failed { re: id, error: e.to_string() },
                };
                me.out.push(reply);
            });
        }
        Request::Upload { id, job, from, name } => {
            let me2 = me.clone();
            tokio::spawn(async move {
                let got = tokio::task::spawn_blocking(move || upload(&me2, job, &from, &name)).await;
                let reply = match got {
                    Ok(Ok(())) => Reply::Done { re: id, code: 0, stdout: String::new(), stderr: String::new() },
                    Ok(Err(e)) => Reply::Done { re: id, code: 1, stdout: String::new(), stderr: format!("{e:#}") },
                    Err(e) => Reply::Failed { re: id, error: e.to_string() },
                };
                me.out.push(reply);
            });
        }
    }
}

/// The most of a script's output kept for a whole answer.
const MAX_OUTPUT: usize = 4 * 1024 * 1024;

async fn sh(me: &Arc<Builder>, id: u64, script: &str, stream: bool, timeout_s: u64) -> anyhow::Result<Reply> {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-s")
        .current_dir(crate::util::home_dir())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().context("starting sh")?;
    let mut stdin = child.stdin.take().context("stdin")?;
    stdin.write_all(script.as_bytes()).await?;
    drop(stdin);
    let (mut so, mut se) = (child.stdout.take().context("stdout")?, child.stderr.take().context("stderr")?);
    if !stream {
        let read = async {
            let (mut a, mut b) = (Vec::new(), Vec::new());
            let (mut ta, mut tb) = ((&mut so).take(MAX_OUTPUT as u64), (&mut se).take(MAX_OUTPUT as u64));
            let _ = tokio::join!(ta.read_to_end(&mut a), tb.read_to_end(&mut b));
            let status = child.wait().await?;
            anyhow::Ok((status, a, b))
        };
        let (status, a, b) = match timeout_s {
            0 => read.await?,
            t => tokio::time::timeout(Duration::from_secs(t), read).await.context("took too long")??,
        };
        return Ok(Reply::Done {
            re: id,
            code: status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&a).into_owned(),
            stderr: String::from_utf8_lossy(&b).into_owned(),
        });
    }
    let (tx, mut stop) = tokio::sync::oneshot::channel();
    me.kills.lock().unwrap_or_else(|e| e.into_inner()).insert(id, tx);
    let mut out = tokio::io::BufReader::new(so).lines();
    let mut err = tokio::io::BufReader::new(se).lines();
    let mut err_open = true;
    let mut last_err = VecDeque::new();
    let status = loop {
        tokio::select! {
            l = out.next_line() => match l {
                Ok(Some(text)) => me.out.push(Reply::Line { re: id, text, err: false }),
                _ => break child.wait().await.ok(),
            },
            l = err.next_line(), if err_open => match l {
                Ok(Some(text)) => {
                    last_err.push_back(text.clone());
                    if last_err.len() > 20 { last_err.pop_front(); }
                    me.out.push(Reply::Line { re: id, text, err: true });
                }
                _ => err_open = false,
            },
            _ = &mut stop => {
                let _ = child.kill().await;
                break None;
            }
        }
    };
    me.kills.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
    Ok(Reply::Done {
        re: id,
        code: status.and_then(|s| s.code()).unwrap_or(-1),
        stdout: String::new(),
        stderr: last_err.into_iter().collect::<Vec<_>>().join("\n"),
    })
}

/// A folder under the builder's home, from the server: relative and plain.
fn under_home(rel: &str) -> anyhow::Result<PathBuf> {
    let r = crate::builds::safe_relative(Path::new(rel))
        .with_context(|| format!("{rel:?} isn't a folder in the builder's home"))?;
    Ok(crate::util::home_dir().join(r))
}

fn client(me: &Builder) -> crate::cli::Client {
    crate::cli::Client::with_token(&me.cfg.url, &me.token).with_timeout(Duration::from_secs(1800))
}

/// Downloads what the store lacks, checking each blob. Returns the bytes received.
fn fetch_blobs(me: &Builder, shas: &[String]) -> anyhow::Result<u64> {
    let st = store();
    let missing = st.missing(shas.iter().map(String::as_str));
    if missing.is_empty() {
        return Ok(0);
    }
    let c = client(me);
    let tmp = tempfile::NamedTempFile::new()?;
    let res = c
        .http
        .post(format!("{}/api/builders/blobs", c.base))
        .bearer_auth(&me.token)
        .json(&json!({"shas": missing}))
        .send()?;
    if !res.status().is_success() {
        bail!("{}: the server didn't send the files", res.status());
    }
    let mut f = std::fs::File::create(tmp.path())?;
    let mut res = res;
    let n = std::io::copy(&mut res, &mut f)?;
    st.unpack(std::fs::File::open(tmp.path())?)?;
    Ok(n)
}

/// Brings a work folder to a snapshot: writes only files whose content changed, deletes files the
/// snapshot no longer has (only files an earlier snapshot put there; builds stay), and moves
/// git's index to the commit, so `git describe` sees what the agent sees.
fn sync(me: &Builder, work: &str, manifest: &str, bundle: &str) -> anyhow::Result<serde_json::Value> {
    crate::blobs::check_sha(manifest)?;
    let mut received = fetch_blobs(me, &[manifest.to_owned()])?;
    let m: Manifest = serde_json::from_slice(&std::fs::read(store().path(manifest))?)?;
    for e in &m.files {
        crate::blobs::check_path(&e.path)?;
        crate::blobs::check_sha(&e.sha)?;
    }
    let mut wanted: Vec<String> = m.files.iter().map(|e| e.sha.clone()).collect();
    if !bundle.is_empty() {
        crate::blobs::check_sha(bundle)?;
        wanted.push(bundle.to_owned());
    }
    received += fetch_blobs(me, &wanted)?;
    let dir = under_home(work)?;
    std::fs::create_dir_all(&dir)?;
    let git = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(&dir).args(args).output();
    if !dir.join(".git").exists() {
        git(&["init", "-q"])?;
    }
    let state_file = dir.join(".git").join("af-manifest.json");
    let before: HashMap<String, String> =
        std::fs::read(&state_file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let st = store();
    let mut changed = 0;
    let mut now = HashMap::new();
    for e in &m.files {
        let target = dir.join(&e.path);
        now.insert(e.path.clone(), e.sha.clone());
        let same =
            before.get(&e.path) == Some(&e.sha) && target.metadata().map(|md| md.len() == e.size).unwrap_or(false);
        if same {
            continue;
        }
        if let Some(p) = target.parent() {
            std::fs::create_dir_all(p)?;
        }
        let tmp = target.with_file_name(format!(".af-{}", e.sha.get(..12).unwrap_or("x")));
        std::fs::copy(st.path(&e.sha), &tmp)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(if e.exec { 0o755 } else { 0o644 }))?;
        std::fs::rename(&tmp, &target)?;
        changed += 1;
    }
    for path in before.keys() {
        if !now.contains_key(path) {
            let _ = std::fs::remove_file(dir.join(path));
            changed += 1;
        }
    }
    std::fs::write(&state_file, serde_json::to_vec(&now)?)?;
    // Git: new commits and tags from the bundle, then the index to the commit (files stay).
    let mut git_ok = false;
    if !m.commit.is_empty() {
        if !bundle.is_empty() {
            let b = st.path(bundle).display().to_string();
            let _ = git(&["fetch", "-q", "--force", &b, "+HEAD:refs/heads/agentflow", "+refs/tags/*:refs/tags/*"]);
        }
        let have =
            git(&["cat-file", "-e", &format!("{}^{{commit}}", m.commit)]).map(|o| o.status.success()).unwrap_or(false);
        if have {
            git_ok = git(&["reset", "-q", "--mixed", &m.commit]).map(|o| o.status.success()).unwrap_or(false);
        }
    }
    let _ = std::fs::File::create(dir.parent().unwrap_or(&dir).join(".used"));
    Ok(json!({"commit": m.commit, "changed": changed, "received": received, "git": git_ok}))
}

/// Packs a path of a finished job and sends it to the server.
fn upload(me: &Builder, job: i64, from: &str, name: &str) -> anyhow::Result<()> {
    let path = under_home(from)?;
    if !path.exists() {
        bail!("{from} isn't there");
    }
    let tmp = tempfile::NamedTempFile::new()?;
    {
        let enc = zstd::stream::write::Encoder::new(std::fs::File::create(tmp.path())?, 3)?.auto_finish();
        let mut tar = tar::Builder::new(enc);
        tar.follow_symlinks(false);
        let base = path.file_name().and_then(|n| n.to_str()).unwrap_or("result");
        if path.is_dir() {
            tar.append_dir_all(base, &path)?;
        } else {
            tar.append_path_with_name(&path, base)?;
        }
        tar.finish()?;
    }
    let c = client(me);
    let file = std::fs::File::open(tmp.path())?;
    let len = file.metadata()?.len();
    let res = c
        .http
        .post(format!("{}/api/builders/results/{job}/{name}", c.base))
        .bearer_auth(&me.token)
        .body(reqwest::blocking::Body::sized(file, len))
        .send()?;
    if !res.status().is_success() {
        bail!("{}: the server didn't take it", res.status());
    }
    Ok(())
}
