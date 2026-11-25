//! Passkeys (WebAuthn, through webauthn-rs). Each belongs to a person (src/people.rs): a device or
//! password manager they enrolled from Settings or from a setup, invite or device link. Signing in
//! needs no username: the passkey says who it is.
//!
//! Browsers allow passkeys only on https (or http://localhost), on a real domain name. On the
//! WireGuard address (http://10.0.8.1) they're unavailable and the page says so; the one-time
//! links from `sudo flow server setup` and `add-device` sign in once instead.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
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
/// At most this many unfinished sign-ins at once, so the public endpoint can't fill memory; when
/// full, the oldest is dropped (refusing would let anyone lock the owner out).
const MAX_PENDING: usize = 64;
/// And at most this many per client address.
const MAX_PER_CLIENT: usize = 4;

enum Pending {
    /// An enrolment, with the WebAuthn user handle it's for (someone new has no row yet).
    Register(PasskeyRegistration, Uuid),
    Login(PasskeyAuthentication),
}

struct Ceremony {
    expires: i64,
    client: String,
    state: Pending,
}

#[derive(Clone)]
pub struct Passkeys {
    webauthn: Option<Arc<Webauthn>>,
    /// Sign-ins (public) and enrolments (owner only) are kept apart, so one can't crowd out the other.
    logins: Arc<Mutex<HashMap<String, Ceremony>>>,
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
        Self { webauthn, logins: Arc::default(), enrolments: Arc::default() }
    }

    /// False on plain http to an IP address, where browsers refuse WebAuthn.
    pub fn available(&self) -> bool {
        self.webauthn.is_some()
    }

    fn webauthn(&self) -> anyhow::Result<&Webauthn> {
        self.webauthn.as_deref().context("passkeys need AgentFlow on https with a domain name")
    }

    fn remember(pool: &Mutex<HashMap<String, Ceremony>>, client: &str, state: Pending) -> String {
        let now = now_ms();
        let mut pending = crate::util::lock(pool);
        pending.retain(|_, c| c.expires > now);
        // Per client: drop that client's oldest; overall: drop the oldest of all.
        let oldest = |p: &HashMap<String, Ceremony>, only: Option<&str>| {
            p.iter()
                .filter(|(_, c)| only.is_none_or(|o| c.client == o))
                .min_by_key(|(_, c)| c.expires)
                .map(|(k, _)| k.clone())
        };
        if pending.values().filter(|c| c.client == client).count() >= MAX_PER_CLIENT
            && let Some(k) = oldest(&pending, Some(client))
        {
            pending.remove(&k);
        }
        if pending.len() >= MAX_PENDING
            && let Some(k) = oldest(&pending, None)
        {
            pending.remove(&k);
        }
        let id = new_secret("");
        pending.insert(id.clone(), Ceremony { expires: now + CEREMONY_MS, client: client.to_owned(), state });
        id
    }

    fn take(pool: &Mutex<HashMap<String, Ceremony>>, id: &str) -> Option<Pending> {
        let c = crate::util::lock(pool).remove(id)?;
        (c.expires > now_ms()).then_some(c.state)
    }

    /// Starts enrolling a passkey for a person, or for someone new (`person` None: a fresh user
    /// handle, the row comes when it's finished). `client` keeps one caller from crowding out
    /// another: the person's id, or a link's.
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
        Ok((Self::remember(&self.enrolments, client, Pending::Register(state, user)), challenge))
    }

    /// Checks an enrolment started by the same `client` (the person, or the link). Returns the
    /// passkey and the user handle it was made for; `store` keeps it for the person.
    pub fn finish_registration(
        &self,
        id: &str,
        client: &str,
        credential: &RegisterPublicKeyCredential,
    ) -> anyhow::Result<(Passkey, Uuid)> {
        let c = crate::util::lock(&self.enrolments).remove(id);
        let Some(Ceremony { expires, client: started_by, state: Pending::Register(state, user) }) = c else {
            bail!("this enrolment expired; start again")
        };
        if expires <= now_ms() {
            bail!("this enrolment expired; start again")
        }
        if started_by != client {
            bail!("this enrolment was started somewhere else");
        }
        Ok((self.webauthn()?.finish_passkey_registration(credential, &state)?, user))
    }

    /// Starts a sign-in. None when no passkey is enrolled yet.
    pub fn start_login(
        &self,
        conn: &Connection,
        client: &str,
    ) -> anyhow::Result<Option<(String, RequestChallengeResponse)>> {
        let keys: Vec<Passkey> = load(conn, None)?.into_iter().map(|(_, p)| p).collect();
        if keys.is_empty() {
            return Ok(None);
        }
        let (challenge, state) = self.webauthn()?.start_passkey_authentication(&keys)?;
        Ok(Some((Self::remember(&self.logins, client, Pending::Login(state)), challenge)))
    }

    /// Checks a sign-in and updates the passkey's counter. Returns the passkey's name and its
    /// person.
    pub fn finish_login(
        &self,
        conn: &Connection,
        id: &str,
        credential: &PublicKeyCredential,
    ) -> anyhow::Result<(String, i64)> {
        let Some(Pending::Login(state)) = Self::take(&self.logins, id) else {
            bail!("this sign-in expired; try again")
        };
        let result = self.webauthn()?.finish_passkey_authentication(credential, &state)?;
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
            assert!(p.start_login(c, "x")?.is_none(), "no passkeys yet");
            let me = crate::people::create(c, true, None, Uuid::new_v4())?;
            let (id, challenge) = p.start_registration(c, Some(me), "giosuel", "1")?;
            let json = serde_json::to_value(&challenge)?;
            assert_eq!(json["publicKey"]["rp"]["id"], "flow.giosuel.io");
            // The person's own user handle.
            let handle = crate::people::webauthn_id(c, me)?;
            assert!(matches!(&p.enrolments.lock().unwrap()[&id].state, Pending::Register(_, u) if *u == handle));
            p.enrolments.lock().unwrap().get_mut(&id).unwrap().expires = 0;
            assert!(Passkeys::take(&p.enrolments, &id).is_none());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn strangers_cant_crowd_out_the_owner() {
        let pool = Mutex::new(HashMap::new());
        let mut first = String::new();
        for i in 0..200 {
            let id = Passkeys::remember(&pool, "attacker", Pending::Login(dummy_login()));
            if i == 0 {
                first = id;
            }
        }
        assert!(pool.lock().unwrap().len() <= MAX_PER_CLIENT, "one address holds at most four");
        assert!(Passkeys::take(&pool, &first).is_none(), "its oldest were dropped");
        let owner = Passkeys::remember(&pool, "owner-ip", Pending::Login(dummy_login()));
        assert!(Passkeys::take(&pool, &owner).is_some(), "the owner still gets a sign-in");
    }

    fn dummy_login() -> PasskeyAuthentication {
        // A real authentication state from a throwaway passkey-less ceremony isn't possible, so
        // build one through serde (the crate allows it with danger-allow-state-serialisation).
        serde_json::from_value(serde_json::json!({"ast": {"credentials": [], "policy": "required", "challenge": "AAAAAAAAAAAAAAAAAAAAAA", "appid": null, "allow_backup_eligible_upgrade": true}}))
            .expect("auth state shape")
    }
}
