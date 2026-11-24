//! The server's own GitHub tokens (the owner, 2026-11-13: "yes, so I would set the github token in
//! the agent flow UI settings?"): one for the owner's repositories (merging, tags, reading PRs) and
//! one for other people's (sending contributions). They're kept encrypted in the database with a
//! key in its own file in the data folder (mode 600), so a copy of the database or a backup alone
//! doesn't reveal them. Without them, `gh` uses the login of the user it runs as, as before.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, KeyInit};
use anyhow::{Context, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rusqlite::{Connection, OptionalExtension, params};

/// Which token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitHub {
    /// The owner's own repositories: merge and tag buttons, PR polling, importing PRs.
    Own,
    /// Other people's repositories: contributions the owner sends.
    External,
}

impl GitHub {
    fn key(self) -> &'static str {
        match self {
            GitHub::Own => "github.token.own",
            GitHub::External => "github.token.external",
        }
    }

    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "own" => Ok(GitHub::Own),
            "external" => Ok(GitHub::External),
            _ => bail!("own or external"),
        }
    }
}

fn key_path(data_dir: &Path) -> PathBuf {
    data_dir.join("secret.key")
}

/// The encryption key, made on first use.
fn key(data_dir: &Path) -> anyhow::Result<[u8; 32]> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = key_path(data_dir);
    match std::fs::read(&path) {
        Ok(b) if b.len() == 32 => {
            let mut k = [0u8; 32];
            k.copy_from_slice(&b);
            Ok(k)
        }
        Ok(_) => bail!("{} isn't a key; move it away and enter the tokens again", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(data_dir)?;
            let k = crate::util::random_bytes::<32>();
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
            f.write_all(&k)?;
            f.sync_all()?;
            Ok(k)
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn seal(data_dir: &Path, plain: &str) -> anyhow::Result<String> {
    let cipher = Aes256Gcm::new_from_slice(&key(data_dir)?).map_err(|_| anyhow::anyhow!("aes key"))?;
    let nonce = crate::util::random_bytes::<12>();
    let ct = cipher.encrypt(&nonce.into(), plain.as_bytes()).map_err(|_| anyhow::anyhow!("aes-gcm"))?;
    let mut all = nonce.to_vec();
    all.extend(ct);
    Ok(format!("v1:{}", STANDARD.encode(all)))
}

fn open(data_dir: &Path, sealed: &str) -> anyhow::Result<String> {
    let raw = STANDARD.decode(sealed.strip_prefix("v1:").context("an unknown format")?)?;
    if raw.len() < 13 {
        bail!("too short");
    }
    let (nonce, ct) = raw.split_at(12);
    let nonce: [u8; 12] = nonce.try_into()?;
    let cipher = Aes256Gcm::new_from_slice(&key(data_dir)?).map_err(|_| anyhow::anyhow!("aes key"))?;
    let plain = cipher.decrypt(&nonce.into(), ct).map_err(|_| anyhow::anyhow!("the key doesn't fit (secret.key changed?)"))?;
    Ok(String::from_utf8(plain)?)
}

/// A token as GitHub hands them out: one word of printable characters.
pub fn check_token(token: &str) -> anyhow::Result<()> {
    if token.len() < 20 || token.len() > 300 || !token.chars().all(|c| c.is_ascii_graphic()) {
        bail!("that doesn't look like a GitHub token");
    }
    Ok(())
}

pub fn store(c: &Connection, data_dir: &Path, which: GitHub, token: &str) -> anyhow::Result<()> {
    check_token(token)?;
    c.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![which.key(), seal(data_dir, token)?],
    )?;
    set_cached(which, Some(token.to_owned()));
    Ok(())
}

pub fn remove(c: &Connection, which: GitHub) -> anyhow::Result<()> {
    c.execute("DELETE FROM settings WHERE key = ?1", [which.key()])?;
    set_cached(which, None);
    Ok(())
}

pub fn load(c: &Connection, data_dir: &Path, which: GitHub) -> anyhow::Result<Option<String>> {
    let sealed: Option<String> =
        c.query_row("SELECT value FROM settings WHERE key = ?1", [which.key()], |r| r.get(0)).optional()?;
    sealed.map(|s| open(data_dir, &s)).transpose()
}

static OWN: RwLock<Option<String>> = RwLock::new(None);
static EXTERNAL: RwLock<Option<String>> = RwLock::new(None);

fn slot(which: GitHub) -> &'static RwLock<Option<String>> {
    match which {
        GitHub::Own => &OWN,
        GitHub::External => &EXTERNAL,
    }
}

fn set_cached(which: GitHub, token: Option<String>) {
    *slot(which).write().unwrap_or_else(|e| e.into_inner()) = token;
}

/// The token for `gh`'s environment, if one is set.
pub fn token(which: GitHub) -> Option<String> {
    slot(which).read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Loads both at start.
pub fn load_all(c: &Connection, data_dir: &Path) {
    for which in [GitHub::Own, GitHub::External] {
        match load(c, data_dir, which) {
            Ok(t) => set_cached(which, t),
            Err(e) => tracing::warn!("the {which:?} GitHub token can't be read: {e:#}"),
        }
    }
}

/// `GH_TOKEN` for gh, when the server has a token for the owner's repositories.
pub fn gh_env() -> Vec<(String, String)> {
    token(GitHub::Own).map(|t| vec![("GH_TOKEN".to_owned(), t)]).unwrap_or_default()
}

/// The last four characters, to show which token is set.
pub fn hint(token: &str) -> String {
    token.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn tokens_are_kept_sealed_and_come_back() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().to_owned();
        let db = Db::open_memory();
        db.call_blocking(move |c| {
            let t = "github_pat_11ABCDEFG0123456789_abcdefghijklmnop";
            store(c, &data, GitHub::External, t)?;
            let raw: String = c.query_row("SELECT value FROM settings WHERE key = 'github.token.external'", [], |r| r.get(0))?;
            assert!(!raw.contains("abcdefghijklmnop"), "stored sealed");
            assert_eq!(load(c, &data, GitHub::External)?.as_deref(), Some(t));
            assert_eq!(token(GitHub::External).as_deref(), Some(t));
            assert!(store(c, &data, GitHub::Own, "no spaces allowed here at all").is_err());
            remove(c, GitHub::External)?;
            assert!(load(c, &data, GitHub::External)?.is_none());
            assert!(token(GitHub::External).is_none());
            assert_eq!(hint(t), "mnop");
            Ok(())
        })
        .unwrap();
    }
}
