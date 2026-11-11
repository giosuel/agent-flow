//! Web Push to the owner's devices (the iPhone home-screen app, desktop browsers).
//!
//! Implemented directly on the RustCrypto crates instead of the `web-push` crate, which pulls in an
//! old HTTP stack: VAPID (RFC 8292) signs a short JWT with the server's P-256 key, and the payload
//! is encrypted for the browser with aes128gcm (RFC 8291). The unit tests check the encryption
//! against the RFC's own example.

use std::path::Path;

use aes_gcm::Aes128Gcm;
use aes_gcm::aead::{Aead, KeyInit};
use anyhow::{Context, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hkdf::Hkdf;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::{PublicKey, SecretKey};
use rand::Rng;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::activity::{self, Action, Entry, Via};
use crate::auth::Actor;
use crate::db::Db;
use crate::util::now_ms;

/// The kinds of notification, as the owner chose them (2026-11-11): "new question, done with a
/// build, something to review for me and urgent other calls that the agent thinks is important".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Question,
    Build,
    Review,
    Urgent,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Question, Kind::Build, Kind::Review, Kind::Urgent];

    pub fn key(self) -> &'static str {
        match self {
            Kind::Question => "question",
            Kind::Build => "build",
            Kind::Review => "review",
            Kind::Urgent => "urgent",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Question => "New question",
            Kind::Build => "Build ready to try",
            Kind::Review => "Something to review: a PR, a tag, a guide",
            Kind::Urgent => "Urgent calls from an agent",
        }
    }

    pub fn from_key(key: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.key() == key)
    }
}

/// What the service worker shows.
#[derive(Debug, Clone, Serialize)]
pub struct Notification {
    pub title: String,
    pub body: String,
    /// Opened when the notification is tapped, e.g. `/questions/vinculum/Q-12`.
    pub url: String,
    /// Notifications with the same tag replace each other instead of piling up.
    pub tag: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// VAPID keys

/// The server's VAPID key pair, kept in `vapid.key` in the data directory.
#[derive(Clone)]
pub struct Vapid {
    key: SecretKey,
}

impl Vapid {
    pub fn load_or_create(data_dir: &Path) -> anyhow::Result<Self> {
        let path = data_dir.join("vapid.key");
        if let Ok(text) = std::fs::read_to_string(&path) {
            let bytes = B64.decode(text.trim()).context("vapid.key isn't base64url")?;
            let key = SecretKey::from_slice(&bytes).context("vapid.key isn't a P-256 key")?;
            return Ok(Self { key });
        }
        let key = random_secret_key();
        write_private(&path, &B64.encode(key.to_bytes()))?;
        Ok(Self { key })
    }

    #[cfg(test)]
    pub fn random() -> Self {
        Self { key: random_secret_key() }
    }

    /// The public key browsers need when they subscribe (`applicationServerKey`).
    pub fn public_key_b64(&self) -> String {
        B64.encode(self.key.public_key().to_sec1_bytes())
    }

    /// The `Authorization: vapid t=…, k=…` header for one push service.
    fn authorization(&self, endpoint: &str, subject: &str) -> anyhow::Result<String> {
        let audience = origin_of(endpoint)?;
        let header = B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        // Push services accept at most 24 hours; Apple is strict about it.
        let exp = now_ms() / 1000 + 12 * 3600;
        let claims = B64.encode(serde_json::json!({"aud": audience, "exp": exp, "sub": subject}).to_string());
        let signing_input = format!("{header}.{claims}");
        let signature: Signature = SigningKey::from(&self.key).sign(signing_input.as_bytes());
        Ok(format!("vapid t={signing_input}.{}, k={}", B64.encode(signature.to_bytes()), self.public_key_b64()))
    }
}

fn random_secret_key() -> SecretKey {
    loop {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        // Almost every 32-byte value is a valid scalar; retry the rare one that isn't.
        if let Ok(key) = SecretKey::from_slice(&bytes) {
            return key;
        }
    }
}

fn write_private(path: &Path, text: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    writeln!(f, "{text}")?;
    Ok(())
}

/// `https://web.push.apple.com/QG…` → `https://web.push.apple.com`
fn origin_of(endpoint: &str) -> anyhow::Result<String> {
    let (scheme, rest) = endpoint.split_once("://").context("push endpoint isn't a URL")?;
    if scheme != "https" && scheme != "http" {
        bail!("push endpoint must be http(s)");
    }
    let host = rest.split('/').next().unwrap_or(rest);
    Ok(format!("{scheme}://{host}"))
}

// ---------------------------------------------------------------------------------------------
// Payload encryption (RFC 8291, aes128gcm)

/// Encrypts `plaintext` for one subscription. `as_key` and `salt` are random for every message;
/// they're parameters so the RFC's example can be checked.
fn encrypt(
    ua_public: &[u8],
    auth_secret: &[u8],
    plaintext: &[u8],
    as_key: &SecretKey,
    salt: &[u8; 16],
) -> anyhow::Result<Vec<u8>> {
    let ua_key = PublicKey::from_sec1_bytes(ua_public).context("the browser's p256dh key is invalid")?;
    let as_public = as_key.public_key().to_sec1_bytes();
    let shared = as_key.diffie_hellman(&ua_key);

    // IKM = HKDF(auth_secret, ecdh_secret, "WebPush: info\0" || ua_public || as_public)
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(&as_public);
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth_secret), shared.raw_secret_bytes())
        .expand(&key_info, &mut ikm)
        .map_err(|_| anyhow::anyhow!("hkdf"))?;

    let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
    let mut cek = [0u8; 16];
    let mut nonce = [0u8; 12];
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek).map_err(|_| anyhow::anyhow!("hkdf"))?;
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce).map_err(|_| anyhow::anyhow!("hkdf"))?;

    // One record: the plaintext, then the 0x02 delimiter that marks the last record.
    let mut record = plaintext.to_vec();
    record.push(2);
    let cipher = Aes128Gcm::new_from_slice(&cek).map_err(|_| anyhow::anyhow!("aes key"))?;
    let ciphertext = cipher.encrypt(&nonce.into(), record.as_slice()).map_err(|_| anyhow::anyhow!("aes-gcm"))?;

    // Header: salt, record size, key id length, key id (our public key), then the record.
    let mut body = Vec::with_capacity(16 + 4 + 1 + as_public.len() + ciphertext.len());
    body.extend_from_slice(salt);
    body.extend_from_slice(&4096u32.to_be_bytes());
    body.push(as_public.len() as u8);
    body.extend_from_slice(&as_public);
    body.extend_from_slice(&ciphertext);
    Ok(body)
}

// ---------------------------------------------------------------------------------------------
// Subscriptions and sending

#[derive(Debug, Clone, Deserialize)]
pub struct SubscriptionKeys {
    pub p256dh: String,
    pub auth: String,
}

/// What `PushManager.subscribe()` returns in the browser, as JSON.
#[derive(Debug, Clone, Deserialize)]
pub struct Subscription {
    pub endpoint: String,
    pub keys: SubscriptionKeys,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceRow {
    pub id: i64,
    pub device: String,
    pub created_at: i64,
    pub last_ok_at: Option<i64>,
    pub last_error: Option<String>,
}

pub fn save_subscription(conn: &Connection, sub: &Subscription, device: &str) -> anyhow::Result<()> {
    origin_of(&sub.endpoint)?;
    if !sub.endpoint.starts_with("https://") {
        bail!("push endpoints must use https");
    }
    // Validate the keys now rather than at the first push.
    let p256dh = B64.decode(sub.keys.p256dh.trim_end_matches('=')).context("p256dh isn't base64url")?;
    PublicKey::from_sec1_bytes(&p256dh).context("p256dh isn't a P-256 key")?;
    let auth = B64.decode(sub.keys.auth.trim_end_matches('=')).context("auth isn't base64url")?;
    if auth.len() != 16 {
        bail!("auth must be 16 bytes");
    }
    conn.execute(
        "INSERT INTO push_subscriptions (endpoint, p256dh, auth, device, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (endpoint) DO UPDATE SET p256dh = excluded.p256dh, auth = excluded.auth, device = excluded.device",
        params![sub.endpoint, sub.keys.p256dh, sub.keys.auth, device, now_ms()],
    )?;
    Ok(())
}

pub fn remove_subscription(conn: &Connection, endpoint: &str) -> anyhow::Result<bool> {
    Ok(conn.execute("DELETE FROM push_subscriptions WHERE endpoint = ?1", [endpoint])? > 0)
}

pub fn remove_device(conn: &Connection, id: i64) -> anyhow::Result<bool> {
    Ok(conn.execute("DELETE FROM push_subscriptions WHERE id = ?1", [id])? > 0)
}

pub fn devices(conn: &Connection) -> anyhow::Result<Vec<DeviceRow>> {
    let mut stmt =
        conn.prepare("SELECT id, device, created_at, last_ok_at, last_error FROM push_subscriptions ORDER BY id")?;
    let rows = stmt.query_map([], |r| {
        Ok(DeviceRow {
            id: r.get(0)?,
            device: r.get(1)?,
            created_at: r.get(2)?,
            last_ok_at: r.get(3)?,
            last_error: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn is_enabled(conn: &Connection, kind: Kind) -> anyhow::Result<bool> {
    let v: Option<String> = conn
        .query_row("SELECT value FROM settings WHERE key = ?1", [format!("notify.{}", kind.key())], |r| r.get(0))
        .optional()?;
    Ok(v.as_deref() != Some("off"))
}

pub fn set_enabled(conn: &Connection, kind: Kind, on: bool) -> anyhow::Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![format!("notify.{}", kind.key()), if on { "on" } else { "off" }],
    )?;
    Ok(())
}

/// Sends notifications. Cheap to clone; one HTTP client for all of them.
#[derive(Clone)]
pub struct Pusher {
    db: Db,
    vapid: Vapid,
    subject: String,
    http: reqwest::Client,
}

struct Target {
    endpoint: String,
    p256dh: String,
    auth: String,
}

impl Pusher {
    pub fn new(db: Db, vapid: Vapid) -> Self {
        let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(15)).build().expect("http client");
        // Push services want a contact for the sender; Apple refuses requests without one.
        Self { db, vapid, subject: "mailto:me@giosuel.ch".into(), http }
    }

    pub fn public_key(&self) -> String {
        self.vapid.public_key_b64()
    }

    /// Sends `n` to every device, unless the owner switched `kind` off. Returns how many devices
    /// accepted it. Runs in the background when called through [`Pusher::spawn`].
    pub async fn send(&self, kind: Kind, n: &Notification) -> anyhow::Result<usize> {
        self.send_inner(Some(kind), n).await
    }

    /// For the "Send a test notification" button: goes out whatever the switches say.
    pub async fn send_ignoring_settings(&self, n: &Notification) -> anyhow::Result<usize> {
        self.send_inner(None, n).await
    }

    async fn send_inner(&self, kind: Option<Kind>, n: &Notification) -> anyhow::Result<usize> {
        let targets = self
            .db
            .call(move |c| {
                if let Some(kind) = kind
                    && !is_enabled(c, kind)?
                {
                    return Ok(Vec::new());
                }
                let mut stmt = c.prepare("SELECT endpoint, p256dh, auth FROM push_subscriptions")?;
                let rows =
                    stmt.query_map([], |r| Ok(Target { endpoint: r.get(0)?, p256dh: r.get(1)?, auth: r.get(2)? }))?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })
            .await?;
        let payload = serde_json::to_vec(n)?;
        let mut delivered = 0;
        for t in targets {
            let result = self.send_one(&t, &payload, kind == Some(Kind::Urgent)).await;
            let endpoint = t.endpoint.clone();
            let (summary, severity) = match &result {
                Ok(()) => (format!("push \"{}\" delivered", n.title), "info"),
                Err(e) => (format!("push \"{}\" failed: {e:#}", n.title), "warn"),
            };
            if result.is_ok() {
                delivered += 1;
            }
            let gone = matches!(&result, Err(e) if e.downcast_ref::<Gone>().is_some());
            let error = result.err().map(|e| format!("{e:#}"));
            self.db
                .call(move |c| {
                    if gone {
                        // The browser unsubscribed or the app was removed: forget the device.
                        remove_subscription(c, &endpoint)?;
                    } else if let Some(err) = &error {
                        c.execute(
                            "UPDATE push_subscriptions SET last_error = ?1 WHERE endpoint = ?2",
                            params![err, endpoint],
                        )?;
                    } else {
                        c.execute(
                            "UPDATE push_subscriptions SET last_ok_at = ?1, last_error = NULL WHERE endpoint = ?2",
                            params![now_ms(), endpoint],
                        )?;
                    }
                    Ok(())
                })
                .await?;
            activity::write(
                &self.db,
                Entry::new(&Actor::System("push"), Via::System, Action::Push, summary).severity(severity),
            )
            .await;
        }
        Ok(delivered)
    }

    /// Fire and forget: a slow push service never holds up the request that caused the push.
    pub fn spawn(&self, kind: Kind, n: Notification) {
        let me = self.clone();
        tokio::spawn(async move {
            if let Err(e) = me.send(kind, &n).await {
                tracing::warn!("push failed: {e:#}");
            }
        });
    }

    async fn send_one(&self, t: &Target, payload: &[u8], urgent: bool) -> anyhow::Result<()> {
        let ua_public = B64.decode(t.p256dh.trim_end_matches('='))?;
        let auth = B64.decode(t.auth.trim_end_matches('='))?;
        let mut salt = [0u8; 16];
        rand::rng().fill_bytes(&mut salt);
        let body = encrypt(&ua_public, &auth, payload, &random_secret_key(), &salt)?;
        let res = self
            .http
            .post(&t.endpoint)
            .header("Authorization", self.vapid.authorization(&t.endpoint, &self.subject)?)
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            // Keep undelivered notifications for a day (a phone that's off overnight).
            .header("TTL", "86400")
            .header("Urgency", if urgent { "high" } else { "normal" })
            .body(body)
            .send()
            .await?;
        let status = res.status();
        if status.is_success() {
            return Ok(());
        }
        let text: String = res.text().await.unwrap_or_default().chars().take(200).collect();
        if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
            return Err(Gone.into());
        }
        bail!("{status} {text}")
    }
}

#[derive(Debug)]
struct Gone;

impl std::fmt::Display for Gone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the push service no longer knows this device")
    }
}

impl std::error::Error for Gone {}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Vec<u8> {
        B64.decode(s).unwrap()
    }

    /// RFC 8291, Appendix A.
    #[test]
    fn encryption_matches_rfc_8291_example() {
        let plaintext = b"When I grow up, I want to be a watermelon";
        let as_key = SecretKey::from_slice(&d("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw")).unwrap();
        assert_eq!(
            B64.encode(as_key.public_key().to_sec1_bytes()),
            "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8"
        );
        let ua_public = d("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4");
        let auth = d("BTBZMqHH6r4Tts7J_aSIgg");
        let salt: [u8; 16] = d("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap();
        let body = encrypt(&ua_public, &auth, plaintext, &as_key, &salt).unwrap();
        assert_eq!(
            B64.encode(body),
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        );
    }

    #[test]
    fn vapid_header_is_a_valid_es256_jwt() {
        use p256::ecdsa::VerifyingKey;
        use p256::ecdsa::signature::Verifier;
        let v = Vapid::random();
        let h = v.authorization("https://web.push.apple.com/QGabc", "mailto:me@example.com").unwrap();
        let (t, k) = h.strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap();
        assert_eq!(k, v.public_key_b64());
        let (signing_input, sig) = t.rsplit_once('.').unwrap();
        let claims: serde_json::Value = serde_json::from_slice(&d(signing_input.split('.').nth(1).unwrap())).unwrap();
        assert_eq!(claims["aud"], "https://web.push.apple.com");
        assert_eq!(claims["sub"], "mailto:me@example.com");
        let key = VerifyingKey::from_sec1_bytes(&d(k)).unwrap();
        let sig = Signature::from_slice(&d(sig)).unwrap();
        key.verify(signing_input.as_bytes(), &sig).unwrap();
    }

    #[test]
    fn vapid_key_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let a = Vapid::load_or_create(dir.path()).unwrap();
        let b = Vapid::load_or_create(dir.path()).unwrap();
        assert_eq!(a.public_key_b64(), b.public_key_b64());
    }

    #[test]
    fn subscriptions_are_validated() {
        let db = Db::open_memory();
        let ua = "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
        let sub = |endpoint: &str, p256dh: &str, auth: &str| Subscription {
            endpoint: endpoint.into(),
            keys: SubscriptionKeys { p256dh: p256dh.into(), auth: auth.into() },
        };
        db.call_blocking(|c| {
            assert!(save_subscription(c, &sub("http://x/1", ua, "BTBZMqHH6r4Tts7J_aSIgg"), "d").is_err());
            assert!(save_subscription(c, &sub("https://x/1", "AAAA", "BTBZMqHH6r4Tts7J_aSIgg"), "d").is_err());
            assert!(save_subscription(c, &sub("https://x/1", ua, "AAAA"), "d").is_err());
            save_subscription(c, &sub("https://x/1", ua, "BTBZMqHH6r4Tts7J_aSIgg"), "iPhone")?;
            save_subscription(c, &sub("https://x/1", ua, "BTBZMqHH6r4Tts7J_aSIgg"), "iPhone again")?;
            assert_eq!(devices(c)?.len(), 1);
            assert!(is_enabled(c, Kind::Question)?);
            set_enabled(c, Kind::Question, false)?;
            assert!(!is_enabled(c, Kind::Question)?);
            Ok(())
        })
        .unwrap();
    }

    /// The browser's side of RFC 8291, to check what the server sends.
    fn decrypt(ua_key: &SecretKey, auth: &[u8], body: &[u8]) -> Vec<u8> {
        let salt = &body[..16];
        let idlen = body[20] as usize;
        let as_public = PublicKey::from_sec1_bytes(&body[21..21 + idlen]).unwrap();
        let ciphertext = &body[21 + idlen..];
        let shared = ua_key.diffie_hellman(&as_public);
        let mut key_info = b"WebPush: info\0".to_vec();
        key_info.extend_from_slice(&ua_key.public_key().to_sec1_bytes());
        key_info.extend_from_slice(&as_public.to_sec1_bytes());
        let mut ikm = [0u8; 32];
        Hkdf::<Sha256>::new(Some(auth), shared.raw_secret_bytes()).expand(&key_info, &mut ikm).unwrap();
        let prk = Hkdf::<Sha256>::new(Some(salt), &ikm);
        let (mut cek, mut nonce) = ([0u8; 16], [0u8; 12]);
        prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek).unwrap();
        prk.expand(b"Content-Encoding: nonce\0", &mut nonce).unwrap();
        let mut plain = Aes128Gcm::new_from_slice(&cek).unwrap().decrypt(&nonce.into(), ciphertext).unwrap();
        assert_eq!(plain.pop(), Some(2), "last-record delimiter");
        plain
    }

    #[tokio::test]
    async fn push_reaches_a_push_service_and_respects_switches() {
        use axum::body::Bytes;
        use axum::http::HeaderMap;
        use std::sync::{Arc, Mutex};

        type Seen = Arc<Mutex<Vec<(HeaderMap, Bytes)>>>;
        let seen: Seen = Arc::default();
        let app = axum::Router::new()
            .route(
                "/push/{id}",
                axum::routing::post(
                    |axum::extract::Path(id): axum::extract::Path<String>,
                     axum::extract::State(seen): axum::extract::State<Seen>,
                     headers: HeaderMap,
                     body: Bytes| async move {
                        if id == "gone" {
                            return axum::http::StatusCode::GONE;
                        }
                        seen.lock().unwrap().push((headers, body));
                        axum::http::StatusCode::CREATED
                    },
                ),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let db = Db::open_memory();
        let ua_key = random_secret_key();
        let auth = [7u8; 16];
        let (p256dh, auth_b64) = (B64.encode(ua_key.public_key().to_sec1_bytes()), B64.encode(auth));
        // Inserted directly: real subscriptions must be https, the stand-in is plain http.
        db.call_blocking(|c| {
            for id in ["phone", "gone"] {
                c.execute(
                    "INSERT INTO push_subscriptions (endpoint, p256dh, auth, device, created_at) VALUES (?1, ?2, ?3, 'd', 0)",
                    params![format!("http://{addr}/push/{id}"), p256dh, auth_b64],
                )?;
            }
            Ok(())
        })
        .unwrap();

        let pusher = Pusher::new(db.clone(), Vapid::random());
        let n =
            Notification { title: "Vinculum asks".into(), body: "Tabs A or B?".into(), url: "/q/1".into(), tag: None };
        assert_eq!(pusher.send(Kind::Urgent, &n).await.unwrap(), 1);
        {
            let seen = seen.lock().unwrap();
            let (headers, body) = &seen[0];
            assert_eq!(headers["content-encoding"], "aes128gcm");
            assert_eq!(headers["urgency"], "high");
            assert!(headers["authorization"].to_str().unwrap().starts_with("vapid t="));
            let json: serde_json::Value = serde_json::from_slice(&decrypt(&ua_key, &auth, body)).unwrap();
            assert_eq!(json["title"], "Vinculum asks");
            assert_eq!(json["url"], "/q/1");
        }
        // The device the push service no longer knows is forgotten.
        assert_eq!(db.call_blocking(|c| devices(c)).unwrap().len(), 1);

        // Switched off: nothing is sent.
        db.call_blocking(|c| set_enabled(c, Kind::Question, false)).unwrap();
        assert_eq!(pusher.send(Kind::Question, &n).await.unwrap(), 0);
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(pusher.send_ignoring_settings(&n).await.unwrap(), 1);
    }
}
