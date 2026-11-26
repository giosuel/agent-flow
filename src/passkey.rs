//! Passkeys (WebAuthn, through webauthn-rs). Each belongs to a person (src/people.rs): a device or
//! password manager they enrolled from Settings or from a setup, invite or device link. Signing in
//! needs no username: the passkey says who it is.
//!
//! Browsers allow passkeys only on https (or http://localhost), on a real domain name. On the
//! WireGuard address (http://10.0.8.1) they're unavailable and the page says so; links from
//! `sudo flow server setup` and `add-device` sign in once instead.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, KeyInit};
use anyhow::{Context, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rusqlite::{Connection, params};
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
/// At most this many unfinished enrolments at once; when full, the oldest is dropped. Only a
/// signed-in person or a link's holder starts them.
const MAX_PENDING: usize = 64;

struct Ceremony {
    expires: i64,
    /// Who started it: "person-3", or a link's hash; only they may finish it.
    client: String,
    state: PasskeyRegistration,
    /// The WebAuthn user handle it's for (someone new has no row yet).
    user: Uuid,
}

#[derive(Clone)]
pub struct Passkeys {
    webauthn: Option<Arc<Webauthn>>,
    /// Sign-ins are public, so the server keeps nothing for one that was only started: its state
    /// travels in the id, sealed with this key (made at start-up; a restart ends the sign-ins
    /// under way, as it always did). A pool of them had to drop someone when full, and anyone
    /// with many addresses, or a local process naming its own, could make that the owner.
    login_key: Arc<[u8; 32]>,
    /// The sign-ins that succeeded, until they'd expire anyway, so each id works once.
    spent: Arc<Mutex<HashMap<Vec<u8>, i64>>>,
    enrolments: Arc<Mutex<HashMap<String, Ceremony>>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PasskeyRow {
    pub id: i64,
    pub name: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub person: Option<i64>,
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
        Self {
            webauthn,
            login_key: Arc::new(crate::util::random_bytes::<32>()),
            spent: Arc::default(),
            enrolments: Arc::default(),
        }
    }

    /// False on plain http to an IP address, where browsers refuse WebAuthn.
    pub fn available(&self) -> bool {
        self.webauthn.is_some()
    }

    fn webauthn(&self) -> anyhow::Result<&Webauthn> {
        self.webauthn.as_deref().context("passkeys need AgentFlow on https with a domain name")
    }

    fn remember(
        pool: &Mutex<HashMap<String, Ceremony>>,
        client: &str,
        state: PasskeyRegistration,
        user: Uuid,
    ) -> String {
        let now = now_ms();
        let mut pending = crate::util::lock(pool);
        pending.retain(|_, c| c.expires > now);
        if pending.len() >= MAX_PENDING
            && let Some(k) = pending.iter().min_by_key(|(_, c)| c.expires).map(|(k, _)| k.clone())
        {
            pending.remove(&k);
        }
        let id = new_secret("");
        pending.insert(id.clone(), Ceremony { expires: now + CEREMONY_MS, client: client.to_owned(), state, user });
        id
    }

    fn take(pool: &Mutex<HashMap<String, Ceremony>>, id: &str) -> Option<Ceremony> {
        let c = crate::util::lock(pool).remove(id)?;
        (c.expires > now_ms()).then_some(c)
    }

    fn cipher(&self) -> anyhow::Result<Aes256Gcm> {
        Aes256Gcm::new_from_slice(&self.login_key[..]).map_err(|_| anyhow::anyhow!("aes key"))
    }

    /// A started sign-in as its id: when it expires and its state, sealed.
    fn seal_login(&self, state: &PasskeyAuthentication, expires: i64) -> anyhow::Result<String> {
        let plain = serde_json::to_vec(&(expires, state))?;
        let nonce = crate::util::random_bytes::<12>();
        let sealed = self.cipher()?.encrypt(&nonce.into(), plain.as_slice()).map_err(|_| anyhow::anyhow!("aes-gcm"))?;
        let mut all = nonce.to_vec();
        all.extend(sealed);
        Ok(URL_SAFE_NO_PAD.encode(all))
    }

    /// The state of a sign-in this server started, unless it expired or was used.
    fn open_login(&self, id: &str) -> Option<(PasskeyAuthentication, i64)> {
        let raw = URL_SAFE_NO_PAD.decode(id).ok()?;
        if raw.len() < 13 || crate::util::lock(&self.spent).contains_key(&crate::util::hash_secret(id)) {
            return None;
        }
        let (nonce, sealed) = raw.split_at(12);
        let nonce: [u8; 12] = nonce.try_into().ok()?;
        let plain = self.cipher().ok()?.decrypt(&nonce.into(), sealed).ok()?;
        let (expires, state): (i64, PasskeyAuthentication) = serde_json::from_slice(&plain).ok()?;
        (expires > now_ms()).then_some((state, expires))
    }

    /// Marks a sign-in as used. False when it was used already. Only sign-ins that passed the
    /// passkey's check get here, so strangers can't grow the list.
    fn spend(&self, id: &str, expires: i64) -> bool {
        let now = now_ms();
        let mut spent = crate::util::lock(&self.spent);
        spent.retain(|_, e| *e > now);
        spent.insert(crate::util::hash_secret(id), expires).is_none()
    }

    /// Starts enrolling a passkey for a person, or for someone new (`person` None: a fresh user
    /// handle, the row comes when it's finished). `client` keeps one caller from finishing
    /// another's: the person ("person-3"), or the link.
    pub fn start_registration(
        &self,
        conn: &Connection,
        person: Option<i64>,
        label: &str,
        client: &str,
    ) -> anyhow::Result<(String, CreationChallengeResponse)> {
        let (user, existing) = match person {
            Some(p) => (
                crate::people::webauthn_id(conn, p)?,
                load(conn, Some(p))?.into_iter().map(|(_, k)| k.cred_id().clone()).collect::<Vec<_>>(),
            ),
            None => (Uuid::new_v4(), Vec::new()),
        };
        let label = if label.trim().is_empty() { "AgentFlow" } else { label.trim() };
        let (challenge, state) = self.webauthn()?.start_passkey_registration(user, label, label, Some(existing))?;
        Ok((Self::remember(&self.enrolments, client, state, user), challenge))
    }

    /// Checks an enrolment started by the same `client`. Returns the passkey and the user handle
    /// it was made for; `store` keeps it for the person.
    pub fn finish_registration(
        &self,
        id: &str,
        client: &str,
        credential: &RegisterPublicKeyCredential,
    ) -> anyhow::Result<(Passkey, Uuid)> {
        let Some(c) = Self::take(&self.enrolments, id) else { bail!("this enrolment expired; start again") };
        if c.client != client {
            bail!("this enrolment was started somewhere else");
        }
        Ok((self.webauthn()?.finish_passkey_registration(credential, &c.state)?, c.user))
    }

    /// Starts a sign-in. None when no passkey is enrolled yet.
    pub fn start_login(&self, conn: &Connection) -> anyhow::Result<Option<(String, RequestChallengeResponse)>> {
        // Only passkeys of people who are here (the security review): not removed, not orphaned.
        let mut keys: Vec<Passkey> = Vec::new();
        for (row, key) in load(conn, None)? {
            if let Some(p) = row.person
                && crate::people::get(conn, p)?.is_some()
            {
                keys.push(key);
            }
        }
        if keys.is_empty() {
            return Ok(None);
        }
        let (challenge, state) = self.webauthn()?.start_passkey_authentication(&keys)?;
        Ok(Some((self.seal_login(&state, now_ms() + CEREMONY_MS)?, challenge)))
    }

    /// Checks a sign-in and updates the passkey's counter. Returns the passkey's name and its
    /// person.
    pub fn finish_login(
        &self,
        conn: &Connection,
        id: &str,
        credential: &PublicKeyCredential,
    ) -> anyhow::Result<(String, i64)> {
        let Some((state, expires)) = self.open_login(id) else { bail!("this sign-in expired; try again") };
        let result = self.webauthn()?.finish_passkey_authentication(credential, &state)?;
        if !self.spend(id, expires) {
            bail!("this sign-in expired; try again");
        }
        let (row, mut passkey) = load(conn, None)?
            .into_iter()
            .find(|(_, p)| p.cred_id() == result.cred_id())
            .context("that passkey was removed")?;
        let person = row.person.context("that passkey belongs to no one")?;
        if crate::people::get(conn, person)?.is_none() {
            bail!("that passkey's person was removed");
        }
        passkey.update_credential(&result);
        conn.execute(
            "UPDATE passkeys SET passkey = ?1, last_used_at = ?2 WHERE id = ?3",
            params![serde_json::to_string(&passkey)?, now_ms(), row.id],
        )?;
        Ok((row.name, person))
    }
}

/// Keeps a checked passkey for a person. Returns its row id.
pub fn store(conn: &Connection, person: i64, name: &str, passkey: &Passkey) -> anyhow::Result<i64> {
    let name = match name.trim() {
        "" => "Passkey".to_owned(),
        n => n.chars().take(60).collect(),
    };
    let cred_id: &[u8] = passkey.cred_id().as_ref();
    conn.execute(
        "INSERT INTO passkeys (cred_id, name, passkey, created_at, person_id) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![cred_id, name, serde_json::to_string(passkey)?, now_ms(), person],
    )?;
    Ok(conn.last_insert_rowid())
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

/// Everyone's passkeys, or one person's.
fn load(conn: &Connection, person: Option<i64>) -> anyhow::Result<Vec<(PasskeyRow, Passkey)>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, created_at, last_used_at, passkey, person_id FROM passkeys
         WHERE ?1 IS NULL OR person_id = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map([person], |r| {
        Ok((
            PasskeyRow {
                id: r.get(0)?,
                name: r.get(1)?,
                created_at: r.get(2)?,
                last_used_at: r.get(3)?,
                person: r.get(5)?,
            },
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

/// A person's passkeys.
pub fn list(conn: &Connection, person: i64) -> anyhow::Result<Vec<PasskeyRow>> {
    Ok(load(conn, Some(person))?.into_iter().map(|(r, _)| r).collect())
}

/// Everyone's, for the sign-in page (any passkey at all?).
pub fn count(conn: &Connection) -> anyhow::Result<i64> {
    Ok(conn.query_row("SELECT count(*) FROM passkeys", [], |r| r.get(0))?)
}

/// Removes one of the person's own passkeys; another's is left alone.
pub fn remove(conn: &Connection, id: i64, person: i64) -> anyhow::Result<bool> {
    Ok(conn.execute("DELETE FROM passkeys WHERE id = ?1 AND person_id = ?2", [id, person])? > 0)
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
            let me = crate::people::create(c, true, None, Uuid::new_v4())?;
            let (id, challenge) = p.start_registration(c, Some(me), "giosuel", "person-1")?;
            let json = serde_json::to_value(&challenge)?;
            assert_eq!(json["publicKey"]["rp"]["id"], "flow.giosuel.io");
            // The person's own user handle, every time.
            let handle = crate::people::webauthn_id(c, me)?;
            assert_eq!(crate::people::webauthn_id(c, me)?, handle);
            assert_eq!(p.enrolments.lock().unwrap()[&id].user, handle);
            p.enrolments.lock().unwrap().get_mut(&id).unwrap().expires = 0;
            assert!(Passkeys::take(&p.enrolments, &id).is_none());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn strangers_cant_crowd_out_the_owner() {
        let p = Passkeys::new(&config("https://flow.giosuel.io"));
        let expires = now_ms() + CEREMONY_MS;
        let owner = p.seal_login(&dummy_login(), expires).unwrap();
        // However many sign-ins others start, from however many addresses: nothing is kept for
        // them, so nothing of the owner's is dropped.
        for _ in 0..1000 {
            p.seal_login(&dummy_login(), expires).unwrap();
        }
        assert!(p.spent.lock().unwrap().is_empty());
        assert!(p.open_login(&owner).is_some(), "the owner's sign-in was dropped");
    }

    #[test]
    fn a_sign_in_is_this_servers_unexpired_and_used_once() {
        let p = Passkeys::new(&config("https://flow.giosuel.io"));
        let expires = now_ms() + CEREMONY_MS;
        let id = p.seal_login(&dummy_login(), expires).unwrap();
        assert!(p.open_login(&id).is_some());
        assert!(p.open_login("").is_none() && p.open_login("AAAA").is_none() && p.open_login("not base64!").is_none());
        // Changed on the way, or sealed by another server (another key).
        let mut other = id.clone().into_bytes();
        let last = other.len() - 2;
        other[last] = if other[last] == b'A' { b'B' } else { b'A' };
        assert!(p.open_login(std::str::from_utf8(&other).unwrap()).is_none());
        assert!(Passkeys::new(&config("https://flow.giosuel.io")).open_login(&id).is_none());
        let old = p.seal_login(&dummy_login(), now_ms() - 1).unwrap();
        assert!(p.open_login(&old).is_none(), "expired");
        // Once it signed someone in, it's used up.
        assert!(p.spend(&id, expires));
        assert!(!p.spend(&id, expires));
        assert!(p.open_login(&id).is_none());
    }

    fn dummy_login() -> PasskeyAuthentication {
        // A real authentication state from a throwaway passkey-less ceremony isn't possible, so
        // build one through serde (the crate allows it with danger-allow-state-serialisation).
        serde_json::from_value(serde_json::json!({"ast": {"credentials": [], "policy": "required", "challenge": "AAAAAAAAAAAAAAAAAAAAAA", "appid": null, "allow_backup_eligible_upgrade": true}}))
            .expect("auth state shape")
    }
}
