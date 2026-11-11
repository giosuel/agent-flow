//! Who is asking. Every request carries one identity: the owner (a browser session), one of the
//! owner's devices, a project's agent, or the consultant (admin). There is no anonymous access to
//! anything but the sign-in page and static files.

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, params};

use crate::util::{hash_secret, new_secret, now_ms};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actor {
    /// The owner in a browser. `method` is how the session started: `passkey` or `link`.
    Owner {
        session_id: i64,
        method: String,
    },
    /// One of the owner's machines: the CLI, viber-run.
    Device {
        token_id: i64,
        name: String,
    },
    /// A project's agent. It may change its own project and read everything else.
    Project {
        token_id: i64,
        project_id: i64,
        slug: String,
    },
    /// The consultant: may change anything, but never press the owner's buttons.
    Admin {
        token_id: i64,
        name: String,
    },
    /// AgentFlow itself: pollers, scheduled jobs, the local CLI working on the database.
    System(&'static str),
    Anonymous,
}

impl Actor {
    pub fn kind(&self) -> &'static str {
        match self {
            Actor::Owner { .. } => "owner",
            Actor::Device { .. } => "device",
            Actor::Project { .. } => "project",
            Actor::Admin { .. } => "admin",
            Actor::System(_) => "system",
            Actor::Anonymous => "anonymous",
        }
    }

    pub fn name(&self) -> String {
        match self {
            Actor::Owner { .. } => "owner".into(),
            Actor::Device { name, .. } | Actor::Admin { name, .. } => name.clone(),
            Actor::Project { slug, .. } => slug.clone(),
            Actor::System(what) => (*what).into(),
            Actor::Anonymous => "anonymous".into(),
        }
    }

    pub fn project(&self) -> Option<&str> {
        match self {
            Actor::Project { slug, .. } => Some(slug),
            _ => None,
        }
    }

    pub fn is_owner(&self) -> bool {
        matches!(self, Actor::Owner { .. })
    }

    /// Owner-only actions (merge, push a tag, new project, settings) need a real browser session.
    /// No token can do them, not even the consultant's.
    pub fn require_owner(&self) -> Result<(), Forbidden> {
        if self.is_owner() { Ok(()) } else { Err(Forbidden("only the owner can do this in the browser")) }
    }

    /// May this identity change records of `project_id`?
    pub fn can_write_project(&self, project_id: i64) -> bool {
        match self {
            Actor::Owner { .. } | Actor::Admin { .. } | Actor::System(_) => true,
            Actor::Project { project_id: own, .. } => *own == project_id,
            Actor::Device { .. } | Actor::Anonymous => false,
        }
    }

    /// The owner's display line in the header, e.g. "giosuel · passkey".
    pub fn label(&self) -> String {
        match self {
            Actor::Owner { method, .. } => format!("owner · {method}"),
            other => format!("{} · {}", other.name(), other.kind()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Forbidden(pub &'static str);

impl std::fmt::Display for Forbidden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Forbidden {}

// ---------------------------------------------------------------------------------------------
// API tokens

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Device,
    Project,
    Admin,
}

impl TokenKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TokenKind::Device => "device",
            TokenKind::Project => "project",
            TokenKind::Admin => "admin",
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            TokenKind::Device => "af_dev_",
            TokenKind::Project => "af_prj_",
            TokenKind::Admin => "af_adm_",
        }
    }
}

impl std::str::FromStr for TokenKind {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(match s {
            "device" => TokenKind::Device,
            "project" => TokenKind::Project,
            "admin" => TokenKind::Admin,
            _ => bail!("token kind must be device, project or admin"),
        })
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TokenInfo {
    pub id: i64,
    pub kind: String,
    pub name: String,
    pub project: Option<String>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub last_used_from: Option<String>,
    pub revoked_at: Option<i64>,
}

/// Creates a token and returns `(id, secret)`. The secret is shown once and never stored.
/// A project token needs its project to exist.
pub fn create_token(
    conn: &Connection,
    kind: TokenKind,
    name: &str,
    project: Option<&str>,
) -> anyhow::Result<(i64, String)> {
    let name = name.trim();
    if name.is_empty() {
        bail!("a token needs a name, e.g. the machine or project it's for");
    }
    let project_id = match (kind, project) {
        (TokenKind::Project, Some(slug)) => Some(
            conn.query_row("SELECT id FROM projects WHERE slug = ?1", [slug], |r| r.get::<_, i64>(0))
                .optional()?
                .with_context(|| format!("no project {slug:?}"))?,
        ),
        (TokenKind::Project, None) => bail!("a project token needs --project"),
        (_, Some(_)) => bail!("only project tokens belong to a project"),
        (_, None) => None,
    };
    let secret = new_secret(kind.prefix());
    conn.execute(
        "INSERT INTO tokens (kind, name, project_id, hash, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![kind.as_str(), name, project_id, hash_secret(&secret), now_ms()],
    )?;
    Ok((conn.last_insert_rowid(), secret))
}

pub fn list_tokens(conn: &Connection) -> anyhow::Result<Vec<TokenInfo>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.kind, t.name, p.slug, t.created_at, t.last_used_at, t.last_used_from, t.revoked_at
         FROM tokens t LEFT JOIN projects p ON p.id = t.project_id ORDER BY t.id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(TokenInfo {
            id: r.get(0)?,
            kind: r.get(1)?,
            name: r.get(2)?,
            project: r.get(3)?,
            created_at: r.get(4)?,
            last_used_at: r.get(5)?,
            last_used_from: r.get(6)?,
            revoked_at: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn revoke_token(conn: &Connection, id: i64) -> anyhow::Result<bool> {
    let n =
        conn.execute("UPDATE tokens SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL", params![now_ms(), id])?;
    Ok(n == 1)
}

/// Looks up a bearer token and records where it was used from.
pub fn token_actor(conn: &Connection, secret: &str, from: &str) -> anyhow::Result<Option<Actor>> {
    let row = conn
        .query_row(
            "SELECT t.id, t.kind, t.name, t.project_id, p.slug FROM tokens t LEFT JOIN projects p ON p.id = t.project_id
             WHERE t.hash = ?1 AND t.revoked_at IS NULL",
            [hash_secret(secret)],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((id, kind, name, project_id, slug)) = row else { return Ok(None) };
    conn.execute(
        "UPDATE tokens SET last_used_at = ?1, last_used_from = ?2 WHERE id = ?3",
        params![now_ms(), from, id],
    )?;
    Ok(Some(match kind.as_str() {
        "device" => Actor::Device { token_id: id, name },
        "admin" => Actor::Admin { token_id: id, name },
        _ => Actor::Project {
            token_id: id,
            project_id: project_id.context("project token without a project")?,
            slug: slug.unwrap_or_default(),
        },
    }))
}

// ---------------------------------------------------------------------------------------------
// One-time links and browser sessions

pub const LINK_MINUTES: i64 = 15;

/// Creates a one-time sign-in link secret, valid for 15 minutes.
pub fn create_login_link(conn: &Connection, purpose: &str) -> anyhow::Result<String> {
    // The prefix lets the log's redaction recognise it anywhere.
    let secret = new_secret("af_lnk_");
    let now = now_ms();
    conn.execute(
        "INSERT INTO login_links (hash, purpose, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
        params![hash_secret(&secret), purpose, now, now + LINK_MINUTES * 60_000],
    )?;
    Ok(secret)
}

/// Uses up a link. Returns its purpose, or None if it's unknown, expired or already used.
pub fn consume_login_link(conn: &Connection, secret: &str) -> anyhow::Result<Option<String>> {
    let now = now_ms();
    Ok(conn
        .query_row(
            "UPDATE login_links SET used_at = ?1 WHERE hash = ?2 AND used_at IS NULL AND expires_at > ?1 RETURNING purpose",
            params![now, hash_secret(secret)],
            |r| r.get(0),
        )
        .optional()?)
}

pub struct SessionLimits {
    pub idle_ms: i64,
    pub max_ms: i64,
}

/// Starts an owner session and returns the cookie secret.
pub fn create_session(
    conn: &Connection,
    method: &str,
    limits: &SessionLimits,
    user_agent: Option<&str>,
    ip: Option<&str>,
) -> anyhow::Result<String> {
    let secret = new_secret("af_ses_");
    let now = now_ms();
    conn.execute(
        "INSERT INTO web_sessions (hash, method, created_at, last_seen_at, expires_at, user_agent, ip)
         VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6)",
        params![hash_secret(&secret), method, now, now + limits.max_ms, user_agent, ip],
    )?;
    Ok(secret)
}

/// Looks up a session cookie. Ends sessions that were idle too long.
pub fn session_actor(conn: &Connection, secret: &str, limits: &SessionLimits) -> anyhow::Result<Option<Actor>> {
    let now = now_ms();
    let row = conn
        .query_row(
            "SELECT id, method, last_seen_at FROM web_sessions
             WHERE hash = ?1 AND revoked_at IS NULL AND expires_at > ?2",
            params![hash_secret(secret), now],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)),
        )
        .optional()?;
    let Some((id, method, last_seen)) = row else { return Ok(None) };
    if now - last_seen > limits.idle_ms {
        conn.execute("UPDATE web_sessions SET revoked_at = ?1 WHERE id = ?2", params![now, id])?;
        return Ok(None);
    }
    // One write a minute is enough to keep the idle clock honest.
    if now - last_seen > 60_000 {
        conn.execute("UPDATE web_sessions SET last_seen_at = ?1 WHERE id = ?2", params![now, id])?;
    }
    Ok(Some(Actor::Owner { session_id: id, method }))
}

pub fn end_session(conn: &Connection, session_id: i64) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE web_sessions SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
        params![now_ms(), session_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn limits() -> SessionLimits {
        SessionLimits { idle_ms: 60 * 60_000, max_ms: 24 * 60 * 60_000 }
    }

    #[test]
    fn token_lifecycle() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            c.execute("INSERT INTO projects (slug, name, created_at) VALUES ('vinculum', 'Vinculum', 0)", [])?;
            assert!(create_token(c, TokenKind::Project, "vinculum agent", None).is_err());
            assert!(create_token(c, TokenKind::Project, "x", Some("nope")).is_err());
            assert!(create_token(c, TokenKind::Device, "x", Some("vinculum")).is_err());

            let (id, secret) = create_token(c, TokenKind::Project, "vinculum agent", Some("vinculum"))?;
            let actor = token_actor(c, &secret, "10.0.8.2")?.unwrap();
            assert_eq!(actor.project(), Some("vinculum"));
            assert!(actor.can_write_project(1));
            assert!(!actor.can_write_project(2));
            assert!(actor.require_owner().is_err());
            assert!(token_actor(c, "af_prj_wrong", "x")?.is_none());

            let info = list_tokens(c)?;
            assert_eq!(info[0].last_used_from.as_deref(), Some("10.0.8.2"));
            assert!(revoke_token(c, id)?);
            assert!(token_actor(c, &secret, "x")?.is_none());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn login_links_work_once() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let link = create_login_link(c, "signin")?;
            assert_eq!(consume_login_link(c, &link)?.as_deref(), Some("signin"));
            assert_eq!(consume_login_link(c, &link)?, None);
            c.execute("UPDATE login_links SET used_at = NULL, expires_at = 0", [])?;
            assert_eq!(consume_login_link(c, &link)?, None, "expired");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn sessions_expire_when_idle() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let s = create_session(c, "link", &limits(), Some("test"), None)?;
            assert!(matches!(session_actor(c, &s, &limits())?, Some(Actor::Owner { .. })));
            c.execute("UPDATE web_sessions SET last_seen_at = 0", [])?;
            assert_eq!(session_actor(c, &s, &limits())?, None);
            Ok(())
        })
        .unwrap();
    }
}
