//! The server's local door for `sudo flow server add-passkey` (the owner, 2026-11-13): a Unix
//! socket only root and the server's own user may use. Whoever can do that has the machine, which
//! is the proof left when every passkey is lost; agents (another user, no sudo) can't. Each link
//! is logged and announced with a push notification, so one the owner didn't ask for shows.
//!
//! On the very first start, while no passkey exists, the server also prints such a link in its
//! log (`journalctl -u agentflow`), which only root and the log's groups read.

use std::path::Path;

use anyhow::Context;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::AppState;
use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::push::{self, Notification};

/// What the link's purpose says, so signing in with it leads straight to adding a passkey.
pub const PURPOSE: &str = "add-passkey";

pub fn spawn(state: AppState) {
    let path = state.config.admin_socket();
    tokio::spawn(async move {
        if let Err(e) = listen(&state, &path).await {
            tracing::warn!("no admin socket at {}: {e:#} (flow server add-passkey won't work)", path.display());
        }
    });
}

async fn listen(state: &AppState, path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = path.parent().context("the socket needs a folder")?;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!("admin socket at {} (sudo flow server add-passkey)", path.display());
    let me = rustix::process::geteuid().as_raw();
    loop {
        let (stream, _) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(&state, stream, me).await {
                tracing::warn!("admin socket: {e:#}");
            }
        });
    }
}

async fn handle(state: &AppState, mut stream: UnixStream, me: u32) -> anyhow::Result<()> {
    let uid = stream.peer_cred()?.uid();
    if uid != 0 && uid != me {
        let e = Entry::new(&Actor::Anonymous, Via::Cli, Action::Denied, format!("admin socket: refused uid {uid}"));
        activity::write(&state.db, e).await;
        stream.write_all(b"{\"error\":\"only root and the server's user may ask (run it with sudo)\"}\n").await?;
        return Ok(());
    }
    let (read, mut write) = stream.split();
    let mut line = String::new();
    let mut reader = BufReader::new(read).take(4096);
    tokio::time::timeout(std::time::Duration::from_secs(10), reader.read_line(&mut line)).await??;
    let req: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
    let reply = if req["request"] == "add_passkey" {
        let url = passkey_link(state, if uid == 0 { "root" } else { "the server's user" }).await?;
        json!({"url": url})
    } else {
        json!({"error": "unknown request"})
    };
    write.write_all(format!("{reply}\n").as_bytes()).await?;
    Ok(())
}

/// Makes a one-time link that signs in and leads to adding a passkey; logs and announces it.
pub async fn passkey_link(state: &AppState, by: &str) -> anyhow::Result<String> {
    let secret = state.db.call(|c| crate::auth::create_login_link(c, PURPOSE)).await?;
    let url = format!("{}/login/link/{secret}", state.config.public_url.trim_end_matches('/'));
    let e = Entry::new(&Actor::System("admin-socket"), Via::Cli, Action::Write, format!("passkey link made on the server, as {by}"));
    activity::write(&state.db, e).await;
    state.push.spawn(
        push::Kind::Urgent,
        Notification {
            title: "A link to add a passkey was made on the server".into(),
            body: format!("As {by}. If it wasn't you, check who has root on the server."),
            url: "/settings".into(),
            tag: Some("add-passkey".into()),
        },
    );
    Ok(url)
}

/// On a server without any passkey yet, the way in: a link in the log.
pub async fn first_start_link(state: &AppState) {
    let none = state.db.call(|c| Ok(crate::passkey::list(c)?.is_empty())).await.unwrap_or(false);
    if !none {
        return;
    }
    match passkey_link(state, "the first start").await {
        Ok(url) => tracing::info!("No passkey yet. Add one (the link works once, for 15 minutes): {url}"),
        Err(e) => tracing::warn!("couldn't make the first passkey link: {e:#}"),
    }
}
