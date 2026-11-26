//! The server's local door for `sudo flow server setup`, `invite` and `add-device` (the owner,
//! 2026-11-13 and 2026-11-25: D-273, D-274): a Unix socket only root may use. Whoever can do that
//! has the machine, which is the proof left when every passkey is lost; agents (another user, no
//! sudo) can't. Each link is logged and announced with a push notification, so one nobody asked
//! for shows.
//!
//! On the very first start, while nobody is set up, the server also prints a setup link in its log
//! (`journalctl -u agentflow`), which only root and the log's groups read.

use std::path::Path;

use anyhow::{Context, bail};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::AppState;
use crate::activity::{self, Action, Entry, Via};
use crate::auth::{self, Actor, purpose};
use crate::people;
use crate::push::{self, Notification};

pub fn spawn(state: AppState) {
    let path = state.config.admin_socket();
    tokio::spawn(async move {
        if let Err(e) = listen(&state, &path).await {
            tracing::warn!("no admin socket at {}: {e:#} (flow server setup won't reach this server)", path.display());
        }
    });
}

async fn listen(state: &AppState, path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = path.parent().context("the socket needs a folder")?;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    claim(path)?;
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!("admin socket at {} (sudo flow server setup)", path.display());
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
    // Only root: where the server runs as the agents' own user (a single-user install), its uid
    // is theirs too, so accepting it would hand any agent the owner's way in.
    let _ = me;
    if uid != 0 {
        let e = Entry::new(&Actor::Anonymous, Via::Cli, Action::Denied, format!("admin socket: refused uid {uid}"));
        activity::write(&state.db, e).await;
        stream.write_all(b"{\"error\":\"only root may ask: run it with sudo\"}\n").await?;
        return Ok(());
    }
    let (read, mut write) = stream.split();
    let mut line = String::new();
    let mut reader = BufReader::new(read).take(4096);
    tokio::time::timeout(std::time::Duration::from_secs(10), reader.read_line(&mut line)).await??;
    let req: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
    let reply = match answer(state, &req).await {
        Ok(v) => v,
        Err(e) => json!({"error": format!("{e:#}")}),
    };
    write.write_all(format!("{reply}\n").as_bytes()).await?;
    Ok(())
}

/// Takes the socket's path for this server. Never another running server's: a test server started
/// as the same user once deleted the live one's, and `sudo flow server …` reached the test server
/// instead (T-237). A socket nobody answers on is a leftover and goes.
fn claim(path: &Path) -> anyhow::Result<()> {
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        bail!("another AgentFlow server already answers there; give this one its own admin_socket in its config");
    }
    let _ = std::fs::remove_file(path);
    Ok(())
}

/// What `flow server …` asks for.
async fn answer(state: &AppState, req: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
    match req["request"].as_str().unwrap_or("") {
        // Who the admin is, so `setup` can say what setting up again removes before asking.
        "status" => {
            let (admin, everyone) = state
                .db
                .call(|c| {
                    let admin = match people::admin(c)? {
                        Some(a) => {
                            let (keys, browsers) = people::counts(c, a.id)?;
                            Some(json!({"name": a.name(), "username": a.username, "passkeys": keys, "browsers": browsers}))
                        }
                        None => None,
                    };
                    Ok((admin, people::list(c)?.len()))
                })
                .await?;
            Ok(json!({"admin": admin, "people": everyone, "url": state.config.public_url}))
        }
        "setup" => {
            // Setting up again needs the admin's username typed back (or "yes" before they picked
            // one): it removes their passkeys and signs out their browsers when the link is used.
            let confirm = req["confirm"].as_str().unwrap_or("").trim().to_owned();
            let again = state
                .db
                .call(move |c| {
                    let Some(a) = people::admin(c)? else { return Ok(false) };
                    let ok = match &a.username {
                        Some(u) => u.eq_ignore_ascii_case(&confirm),
                        None => confirm.eq_ignore_ascii_case("yes"),
                    };
                    if !ok {
                        bail!("that isn't the admin's username; nothing was made");
                    }
                    Ok(true)
                })
                .await?;
            let (url, until) = link(state, purpose::SETUP, None, None, auth::LINK_MINUTES).await?;
            announce(
                state,
                if again {
                    "A link to set up the admin again was made on the server"
                } else {
                    "A setup link was made on the server"
                },
            )
            .await;
            Ok(json!({"url": url, "expires_at": until, "again": again}))
        }
        "invite" => {
            let admin = state
                .db
                .call(|c| people::admin(c))
                .await?
                .context("nobody is set up yet: run `sudo flow server setup` first")?;
            let (url, until) = link(state, purpose::INVITE, None, Some(admin.id), auth::INVITE_MINUTES).await?;
            announce(state, "An invite was made on the server").await;
            Ok(json!({"url": url, "expires_at": until, "by": admin.name()}))
        }
        "add_device" => {
            let who = req["username"].as_str().unwrap_or("").trim().to_owned();
            let person = state
                .db
                .call(move |c| {
                    if who.is_empty() {
                        people::admin(c)?.context("nobody is set up yet: run `sudo flow server setup` first")
                    } else {
                        people::by_username(c, &who)?.with_context(|| format!("nobody here is called {who:?}"))
                    }
                })
                .await?;
            let (url, until) = link(state, purpose::DEVICE, Some(person.id), None, auth::LINK_MINUTES).await?;
            announce(state, &format!("A link to add a device for {} was made on the server", person.name())).await;
            Ok(json!({"url": url, "expires_at": until, "person": person.name(), "admin": person.admin}))
        }
        "add_passkey" => bail!(
            "`flow server add-passkey` is now `flow server add-device` (a new device) or `flow server setup` (the admin lost every passkey)"
        ),
        _ => bail!("unknown request"),
    }
}

/// Makes a one-time link and returns its address and when it stops working.
async fn link(
    state: &AppState,
    kind: &'static str,
    person: Option<i64>,
    made_by: Option<i64>,
    minutes: i64,
) -> anyhow::Result<(String, i64)> {
    let secret = state.db.call(move |c| auth::create_login_link(c, kind, person, made_by, minutes)).await?;
    let url = format!("{}/{kind}/{secret}", state.config.public_url.trim_end_matches('/'));
    Ok((url, crate::util::now_ms() + minutes * 60_000))
}

/// Logs a link made on the server and pushes a notification about it.
async fn announce(state: &AppState, what: &str) {
    let e = Entry::new(&Actor::System("admin-socket"), Via::Cli, Action::Write, format!("{what}, as root"));
    activity::write(&state.db, e).await;
    state.push.spawn(
        push::Kind::Urgent,
        Notification {
            title: what.to_owned(),
            body: "As root. If it wasn't you, check who has root on the server.".into(),
            url: "/settings".into(),
            tag: Some("admin-link".into()),
        },
    );
}

/// On a server nobody is set up on yet, the way in: a setup link in the log.
pub async fn first_start_link(state: &AppState) {
    let none = state.db.call(|c| Ok(people::admin(c)?.is_none())).await.unwrap_or(false);
    // Only when the server runs as its own user: otherwise its log is the agents' to read.
    if !none || state.config.agents_home.is_empty() {
        if none {
            tracing::info!("Nobody is set up yet. Make the admin with a link from: sudo flow server setup");
        }
        return;
    }
    match link(state, purpose::SETUP, None, None, auth::LINK_MINUTES).await {
        Ok((url, _)) => {
            announce(state, "A setup link was made at the first start").await;
            tracing::info!("Nobody is set up yet. Make the admin (the link works once, for 15 minutes): {url}");
        }
        Err(e) => tracing::warn!("couldn't make the first setup link: {e:#}"),
    }
}

/// A setup, invite or device link, as `flow server …` would get it (tests).
#[cfg(test)]
pub async fn test_link(state: &AppState, req: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    answer(state, &req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_running_servers_socket_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("af-claim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("admin.sock");
        let live = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(claim(&path).is_err(), "another server answers there");
        assert!(path.exists());
        drop(live);
        claim(&path).unwrap();
        assert!(!path.exists(), "a leftover goes");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
