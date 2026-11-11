//! The owner's passkeys (WebAuthn, through webauthn-rs). There is one user, the owner; each
//! passkey is a device or password manager they enrolled from Settings.
//!
//! Browsers allow passkeys only on https (or http://localhost), on a real domain name. On the
//! WireGuard address (http://10.0.8.1) they're unavailable and the page says so; the one-time
//! link from `agentflow admin login-link` works everywhere.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use webauthn_rs::prelude::{
    CreationChallengeResponse, Passkey, PasskeyAuthentication, PasskeyRegistration, PublicKeyCredential,
    RegisterPublicKeyCredential, RequestChallengeResponse, Url, Uuid,
};
use webauthn_rs::{Webauthn, WebauthnBuilder};

use crate::config::Config;
use crate::util::{new_secret, now_ms};

/// Ceremonies must finish within this time.
const CEREMONY_MS: i64 = 5 * 60_000;
/// At most this many unfinished ceremonies at once, so the public sign-in endpoint can't be used
/// to fill memory.
const MAX_PENDING: usize = 64;

enum Pending {
    Register(PasskeyRegistration),
    Login(PasskeyAuthentication),
}

#[derive(Clone)]
pub struct Passkeys {
    webauthn: Option<Arc<Webauthn>>,
    pending: Arc<Mutex<HashMap<String, (i64, Pending)>>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PasskeyRow {
    pub id: i64,
    pub name: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

impl Passkeys {
    pub fn new(config: &Config) -> Self {
        let webauthn = match build_webauthn(config) {
            Ok(w) => w.map(Arc::new),
            Err(e) => {
                tracing::warn!("passkeys are off: {e:#}");
                None
            }
        };
        Self { webauthn, pending: Arc::default() }
    }

    /// False on plain http to an IP address, where browsers refuse WebAuthn.
    pub fn available(&self) -> bool {
        self.webauthn.is_some()
    }

    fn webauthn(&self) -> anyhow::Result<&Webauthn> {
        self.webauthn.as_deref().context("passkeys need AgentFlow on https with a domain name")
    }

    fn remember(&self, state: Pending) -> anyhow::Result<String> {
        let now = now_ms();
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|_, (expires, _)| *expires > now);
        if pending.len() >= MAX_PENDING {
            bail!("too many sign-in attempts at once; try again in a few minutes");
        }
        let id = new_secret("");
        pending.insert(id.clone(), (now + CEREMONY_MS, state));
        Ok(id)
    }

    fn take(&self, id: &str) -> Option<Pending> {
        let (expires, state) = self.pending.lock().unwrap().remove(id)?;
        (expires > now_ms()).then_some(state)
    }

    /// Starts enrolling a new passkey (the owner is signed in).
    pub fn start_registration(&self, conn: &Connection) -> anyhow::Result<(String, CreationChallengeResponse)> {
        let user = owner_uuid(conn)?;
        let existing = load(conn)?.into_iter().map(|(_, p)| p.cred_id().clone()).collect::<Vec<_>>();
        let (challenge, state) =
            self.webauthn()?.start_passkey_registration(user, "owner", "AgentFlow owner", Some(existing))?;
        Ok((self.remember(Pending::Register(state))?, challenge))
    }

    pub fn finish_registration(
        &self,
        conn: &Connection,
        id: &str,
        name: &str,
        credential: &RegisterPublicKeyCredential,
    ) -> anyhow::Result<i64> {
        let Some(Pending::Register(state)) = self.take(id) else { bail!("this enrolment expired; start again") };
        let passkey = self.webauthn()?.finish_passkey_registration(credential, &state)?;
        let name = match name.trim() {
            "" => "Passkey".to_owned(),
            n => n.chars().take(60).collect(),
        };
        let cred_id: &[u8] = passkey.cred_id().as_ref();
        conn.execute(
            "INSERT INTO passkeys (cred_id, name, passkey, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![cred_id, name, serde_json::to_string(&passkey)?, now_ms()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Starts a sign-in. None when no passkey is enrolled yet.
    pub fn start_login(&self, conn: &Connection) -> anyhow::Result<Option<(String, RequestChallengeResponse)>> {
        let keys: Vec<Passkey> = load(conn)?.into_iter().map(|(_, p)| p).collect();
        if keys.is_empty() {
            return Ok(None);
        }
        let (challenge, state) = self.webauthn()?.start_passkey_authentication(&keys)?;
        Ok(Some((self.remember(Pending::Login(state))?, challenge)))
    }

    /// Checks a sign-in and updates the passkey's counter. Returns the passkey's name.
    pub fn finish_login(
        &self,
        conn: &Connection,
        id: &str,
        credential: &PublicKeyCredential,
    ) -> anyhow::Result<String> {
        let Some(Pending::Login(state)) = self.take(id) else { bail!("this sign-in expired; try again") };
        let result = self.webauthn()?.finish_passkey_authentication(credential, &state)?;
        let (row, mut passkey) = load(conn)?
            .into_iter()
            .find(|(_, p)| p.cred_id() == result.cred_id())
            .context("that passkey was removed")?;
        passkey.update_credential(&result);
        conn.execute(
            "UPDATE passkeys SET passkey = ?1, last_used_at = ?2 WHERE id = ?3",
            params![serde_json::to_string(&passkey)?, now_ms(), row.id],
        )?;
        Ok(row.name)
    }
}

fn build_webauthn(config: &Config) -> anyhow::Result<Option<Webauthn>> {
    let origin = Url::parse(&config.public_origin())?;
    let host = origin.host_str().context("public_url has no host")?.to_owned();
    let secure = origin.scheme() == "https" || host == "localhost";
    let is_ip = host.parse::<std::net::IpAddr>().is_ok();
    if !secure || is_ip {
        return Ok(None);
    }
    Ok(Some(WebauthnBuilder::new(&host, &origin)?.rp_name("AgentFlow").build()?))
}

/// The owner's WebAuthn user handle, made once and kept in settings.
fn owner_uuid(conn: &Connection) -> anyhow::Result<Uuid> {
    let stored: Option<String> =
        conn.query_row("SELECT value FROM settings WHERE key = 'owner.webauthn_id'", [], |r| r.get(0)).optional()?;
    if let Some(s) = stored {
        return Ok(Uuid::parse_str(&s)?);
    }
    let id = Uuid::new_v4();
    conn.execute("INSERT INTO settings (key, value) VALUES ('owner.webauthn_id', ?1)", [id.to_string()])?;
    Ok(id)
}

fn load(conn: &Connection) -> anyhow::Result<Vec<(PasskeyRow, Passkey)>> {
    let mut stmt = conn.prepare("SELECT id, name, created_at, last_used_at, passkey FROM passkeys ORDER BY id")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            PasskeyRow { id: r.get(0)?, name: r.get(1)?, created_at: r.get(2)?, last_used_at: r.get(3)? },
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (row, json) = row?;
        out.push((row, serde_json::from_str(&json).context("a stored passkey is unreadable")?));
    }
    Ok(out)
}

pub fn list(conn: &Connection) -> anyhow::Result<Vec<PasskeyRow>> {
    Ok(load(conn)?.into_iter().map(|(r, _)| r).collect())
}

pub fn count(conn: &Connection) -> anyhow::Result<i64> {
    Ok(conn.query_row("SELECT count(*) FROM passkeys", [], |r| r.get(0))?)
}

pub fn remove(conn: &Connection, id: i64) -> anyhow::Result<bool> {
    Ok(conn.execute("DELETE FROM passkeys WHERE id = ?1", [id])? > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(url: &str) -> Config {
        Config { public_url: url.into(), ..Config::default() }
    }

    #[test]
    fn passkeys_only_on_secure_named_origins() {
        assert!(Passkeys::new(&config("https://flow.giosuel.io")).available());
        assert!(Passkeys::new(&config("http://localhost:8790")).available());
        assert!(!Passkeys::new(&config("http://10.0.8.1:8790")).available());
        assert!(!Passkeys::new(&config("http://flow.giosuel.io")).available());
    }

    #[test]
    fn registration_starts_and_ceremonies_expire() {
        let db = crate::db::Db::open_memory();
        let p = Passkeys::new(&config("https://flow.giosuel.io"));
        db.call_blocking(|c| {
            assert!(p.start_login(c)?.is_none(), "no passkeys yet");
            let (id, challenge) = p.start_registration(c)?;
            let json = serde_json::to_value(&challenge)?;
            assert_eq!(json["publicKey"]["rp"]["id"], "flow.giosuel.io");
            // The same user handle every time.
            assert_eq!(owner_uuid(c)?, owner_uuid(c)?);
            p.pending.lock().unwrap().get_mut(&id).unwrap().0 = 0;
            assert!(p.take(&id).is_none());
            Ok(())
        })
        .unwrap();
    }
}
