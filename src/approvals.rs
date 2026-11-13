//! Approvals: what `flow` asks for and the owner approves on the page, signed in with their
//! passkey (the owner, 2026-11-13). Connecting a machine (`flow connect`) and the owner's admin
//! jobs (tokens, projects, backups, importing PRs): a token on disk is never enough for those,
//! because agents running as the same Unix user could read it.
//!
//! `flow` sends a request and gets a short code, shown on both sides so the owner can tell it's
//! theirs, and a poll secret only it knows (stored hashed). The owner opens `/approve/<code>`,
//! sees what is asked, from which machine and address, and approves or denies. On approval the
//! server does it; a result such as a new token is handed to the poller once, then cleared.

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::{self, TokenKind};
use crate::util::{hash_secret, new_secret, now_ms};

/// How long a request waits for the owner.
pub const MINUTES: i64 = 10;
/// At most this many requests wait at once: anyone may ask to connect, so a flood is cut off.
pub const MAX_PENDING: i64 = 30;

/// What can be asked for.
pub const ACTIONS: &[&str] =
    &["connect", "token_create", "token_revoke", "token_rotate", "project_add", "backup", "import_prs"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    pub id: i64,
    pub code: String,
    pub action: String,
    pub args: Value,
    pub machine: String,
    pub requested_by: String,
    pub ip: Option<String>,
    pub state: String,
    pub error: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
}

/// A code people compare at a glance: six characters without look-alikes (no 0/O, 1/I/L).
fn new_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    // The poll secret carries the real security; the code only has to be unlikely to repeat.
    let raw = crate::util::random_bytes::<6>();
    let c: String = raw.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect();
    format!("{}-{}", &c[..3], &c[3..])
}

/// A machine's name as flow reports it ("viber@mothman"): short and printable.
pub fn check_machine(machine: &str) -> anyhow::Result<String> {
    let m = machine.trim();
    if m.is_empty() || m.chars().count() > 80 || m.chars().any(|c| c.is_control()) {
        bail!("say which machine this is, e.g. viber@mothman (one line, up to 80 characters)");
    }
    Ok(m.to_owned())
}

/// Checks a request's arguments before anyone is asked, so the owner only sees things that can
/// be done.
pub fn check_args(c: &Connection, action: &str, args: &Value) -> anyhow::Result<()> {
    let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("").trim().to_owned();
    let n = |k: &str| args.get(k).and_then(Value::as_i64);
    match action {
        "connect" | "backup" => Ok(()),
        "token_create" => {
            let kind: TokenKind = s("kind").parse()?;
            match kind {
                TokenKind::Project => {
                    crate::projects::require(c, &s("project"))?;
                }
                TokenKind::Admin => {}
                TokenKind::Device => bail!("a machine's token comes from flow connect"),
            }
            if s("name").is_empty() && kind != TokenKind::Project {
                bail!("give the token a name");
            }
            Ok(())
        }
        "token_revoke" | "token_rotate" => {
            let id = n("id").context("which token (its id, from flow token list)")?;
            let active: bool = c
                .query_row("SELECT revoked_at IS NULL FROM tokens WHERE id = ?1", [id], |r| r.get(0))
                .optional()?
                .unwrap_or(false);
            if !active {
                bail!("no active token {id}");
            }
            Ok(())
        }
        "project_add" => {
            crate::projects::check_slug(&s("slug"))?;
            if s("name").is_empty() || s("name").chars().count() > 60 {
                bail!("give the project a name (up to 60 characters)");
            }
            if crate::projects::get(c, &s("slug"))?.is_some() {
                bail!("project {} exists already", s("slug"));
            }
            Ok(())
        }
        "import_prs" => {
            let p = crate::projects::require(c, &s("project"))?;
            if p.repo.trim().is_empty() {
                bail!("{} has no repository set", p.slug);
            }
            let (from, to) = (n("from").unwrap_or(0), n("to").unwrap_or(0));
            if from < 1 || to < from || to - from > 2000 {
                bail!("give a range of PR numbers, e.g. 300-374");
            }
            Ok(())
        }
        other => bail!("unknown request {other:?}"),
    }
}

/// Records a request. Returns `(code, poll secret)`.
pub fn create(
    c: &Connection,
    action: &str,
    args: &Value,
    machine: &str,
    requested_by: &str,
    ip: Option<&str>,
) -> anyhow::Result<(String, String)> {
    expire(c)?;
    let pending: i64 = c.query_row("SELECT count(*) FROM approvals WHERE state = 'pending'", [], |r| r.get(0))?;
    if pending >= MAX_PENDING {
        bail!("too many requests are waiting for approval; try again in a few minutes");
    }
    check_args(c, action, args)?;
    let machine = check_machine(machine)?;
    let poll = new_secret("af_apv_");
    let now = now_ms();
    // A clash of six characters is unlikely but possible: try a few.
    for _ in 0..5 {
        let code = new_code();
        let done = c.execute(
            "INSERT OR IGNORE INTO approvals (code, poll_hash, action, args, machine, requested_by, ip, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![code, hash_secret(&poll), action, args.to_string(), machine, requested_by, ip, now, now + MINUTES * 60_000],
        )?;
        if done == 1 {
            return Ok((code, poll));
        }
    }
    bail!("couldn't make a code; try again")
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Approval> {
    let args: String = r.get(3)?;
    Ok(Approval {
        id: r.get(0)?,
        code: r.get(1)?,
        action: r.get(2)?,
        args: serde_json::from_str(&args).unwrap_or(Value::Null),
        machine: r.get(4)?,
        requested_by: r.get(5)?,
        ip: r.get(6)?,
        state: r.get(7)?,
        error: r.get(8)?,
        created_at: r.get(9)?,
        expires_at: r.get(10)?,
    })
}

const COLUMNS: &str = "id, code, action, args, machine, requested_by, ip, state, error, created_at, expires_at";

/// Requests past their time are expired, and results nobody fetched are dropped.
pub fn expire(c: &Connection) -> anyhow::Result<()> {
    let now = now_ms();
    c.execute("UPDATE approvals SET state = 'expired' WHERE state = 'pending' AND expires_at <= ?1", [now])?;
    c.execute("UPDATE approvals SET result = NULL WHERE result IS NOT NULL AND expires_at <= ?1", [now])?;
    Ok(())
}

pub fn by_code(c: &Connection, code: &str) -> anyhow::Result<Option<Approval>> {
    expire(c)?;
    let code = code.trim().to_uppercase();
    Ok(c.query_row(&format!("SELECT {COLUMNS} FROM approvals WHERE code = ?1"), [code], row).optional()?)
}

/// The requests waiting now, newest first (the owner's list).
pub fn pending(c: &Connection) -> anyhow::Result<Vec<Approval>> {
    expire(c)?;
    let mut st =
        c.prepare(&format!("SELECT {COLUMNS} FROM approvals WHERE state = 'pending' ORDER BY created_at DESC"))?;
    Ok(st.query_map([], row)?.collect::<Result<_, _>>()?)
}

/// The owner's answer. Only a pending request can be decided; the caller then does it (see
/// [`execute`]) and records the outcome with [`finish`].
pub fn decide(c: &Connection, code: &str, approve: bool) -> anyhow::Result<Approval> {
    let a = by_code(c, code)?.context("no such request")?;
    if a.state != "pending" {
        bail!("this request is {} already", a.state);
    }
    let state = if approve { "approved" } else { "denied" };
    c.execute(
        "UPDATE approvals SET state = ?1, decided_at = ?2 WHERE id = ?3 AND state = 'pending'",
        params![state, now_ms(), a.id],
    )?;
    Ok(Approval { state: state.into(), ..a })
}

/// Records what came of an approved request: its result for the poller, or why it failed.
pub fn finish(c: &Connection, id: i64, outcome: &anyhow::Result<Value>) -> anyhow::Result<()> {
    match outcome {
        Ok(v) => c.execute("UPDATE approvals SET result = ?1 WHERE id = ?2", params![v.to_string(), id])?,
        Err(e) => {
            c.execute("UPDATE approvals SET state = 'failed', error = ?1 WHERE id = ?2", params![format!("{e:#}"), id])?
        }
    };
    Ok(())
}

/// What flow sees when it polls: the state, and once approved, the result (handed out once).
pub fn poll(c: &Connection, secret: &str) -> anyhow::Result<Option<Value>> {
    expire(c)?;
    let found: Option<(i64, String, Option<String>, Option<String>, Option<i64>)> = c
        .query_row(
            "SELECT id, state, result, error, fetched_at FROM approvals WHERE poll_hash = ?1",
            [hash_secret(secret)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((id, state, result, error, fetched)) = found else { return Ok(None) };
    match (state.as_str(), result) {
        ("approved", Some(result)) => {
            c.execute(
                "UPDATE approvals SET state = 'done', result = NULL, fetched_at = ?1 WHERE id = ?2",
                params![now_ms(), id],
            )?;
            let result: Value = serde_json::from_str(&result).unwrap_or(Value::Null);
            Ok(Some(json!({"state": "approved", "result": result})))
        }
        ("done", _) if fetched.is_some() => Ok(Some(json!({"state": "done"}))),
        (state, _) => Ok(Some(json!({"state": state, "error": error}))),
    }
}

/// Does an approved request that needs only the database: everything but importing PRs, which
/// asks GitHub first (see the web handler). The owner approved it, so it's done as the owner.
pub fn execute(c: &mut Connection, a: &Approval, data_dir: &std::path::Path) -> anyhow::Result<Value> {
    let s = |k: &str| a.args.get(k).and_then(Value::as_str).unwrap_or("").trim().to_owned();
    let n = |k: &str| a.args.get(k).and_then(Value::as_i64).unwrap_or(0);
    match a.action.as_str() {
        "connect" => {
            let (id, secret) = auth::create_token(c, TokenKind::Device, &a.machine, None)?;
            Ok(json!({"token": secret, "id": id, "name": a.machine}))
        }
        "token_create" => {
            let kind: TokenKind = s("kind").parse()?;
            let project = (kind == TokenKind::Project).then(|| s("project"));
            let name = if s("name").is_empty() { format!("{} agent", s("project")) } else { s("name") };
            let (id, secret) = auth::create_token(c, kind, &name, project.as_deref())?;
            Ok(json!({"token": secret, "id": id, "name": name}))
        }
        "token_revoke" => {
            if !auth::revoke_token(c, n("id"))? {
                bail!("no active token {}", n("id"));
            }
            Ok(json!({"revoked": n("id")}))
        }
        "token_rotate" => {
            let tx = c.transaction()?;
            let (kind, name, project): (String, String, Option<String>) = tx
                .query_row(
                    "SELECT t.kind, t.name, p.slug FROM tokens t LEFT JOIN projects p ON p.id = t.project_id
                     WHERE t.id = ?1 AND t.revoked_at IS NULL",
                    [n("id")],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?
                .with_context(|| format!("no active token {}", n("id")))?;
            let (id, secret) = auth::create_token(&tx, kind.parse()?, &name, project.as_deref())?;
            auth::revoke_token(&tx, n("id"))?;
            tx.commit()?;
            Ok(json!({"token": secret, "id": id, "name": name, "replaces": n("id")}))
        }
        "project_add" => {
            let p = crate::projects::create(c, &s("slug"), &s("name"), "owner")?;
            Ok(json!({"project": p.slug}))
        }
        "backup" => {
            let now = now_ms();
            let stamp = format!("{}-{}", crate::util::fmt_date(now), crate::util::fmt_hm(now).replace(':', ""));
            let made = crate::backup::backup(c, data_dir, &stamp)?;
            let n = crate::backup::export(c, data_dir)?;
            Ok(json!({"backup": made.map(|p| p.display().to_string()), "exported": n}))
        }
        other => bail!("{other} isn't done here"),
    }
}

/// One line on what a request does, for the page, the log and the push notification.
pub fn describe(a: &Approval) -> String {
    let s = |k: &str| a.args.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
    let n = |k: &str| a.args.get(k).and_then(Value::as_i64).unwrap_or(0);
    match a.action.as_str() {
        "connect" => format!("Connect {} to AgentFlow", a.machine),
        "token_create" if s("kind") == "project" => format!("Create a token for the project {}", s("project")),
        "token_create" => format!("Create a {} token \"{}\"", s("kind"), s("name")),
        "token_revoke" => format!("Revoke token {}", n("id")),
        "token_rotate" => format!("Replace token {} with a new one", n("id")),
        "project_add" => format!("Add the project {} ({})", s("name"), s("slug")),
        "backup" => "Back up the database and export every project".into(),
        "import_prs" => format!("Import PRs #{}–#{} into {}", n("from"), n("to"), s("project")),
        other => other.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn a_connect_goes_from_request_to_token_once() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            let (code, poll) = create(c, "connect", &json!({}), "viber@mothman", "", Some("10.0.8.5"))?;
            assert_eq!(code.len(), 7);
            assert_eq!(super::poll(c, &poll)?.unwrap()["state"], "pending");
            assert!(super::poll(c, "af_apv_wrong")?.is_none());
            let a = decide(c, &code.to_lowercase(), true)?;
            let out = execute(c, &a, std::path::Path::new("/nonexistent"));
            finish(c, a.id, &out)?;
            let got = super::poll(c, &poll)?.unwrap();
            assert_eq!(got["state"], "approved");
            let token = got["result"]["token"].as_str().unwrap().to_owned();
            assert!(token.starts_with("af_dev_"), "{token}");
            assert!(auth::token_actor(c, &token, "x")?.is_some());
            // Handed out once: the secret is gone from the database.
            assert_eq!(super::poll(c, &poll)?.unwrap()["state"], "done");
            let left: Option<String> = c.query_row("SELECT result FROM approvals WHERE id = ?1", [a.id], |r| r.get(0))?;
            assert!(left.is_none());
            assert!(decide(c, &code, true).is_err(), "decided once");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn requests_are_checked_denied_and_expire() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            assert!(create(c, "token_create", &json!({"kind": "project", "project": "nope"}), "m", "d", None).is_err());
            assert!(create(c, "token_create", &json!({"kind": "device", "name": "x"}), "m", "d", None).is_err());
            assert!(create(c, "sudo", &json!({}), "m", "d", None).is_err());
            assert!(create(c, "connect", &json!({}), "", "", None).is_err(), "names the machine");
            let (code, poll) = create(c, "backup", &json!({}), "viber@pc", "pc", None)?;
            decide(c, &code, false)?;
            assert_eq!(super::poll(c, &poll)?.unwrap()["state"], "denied");
            let (_, poll) = create(c, "connect", &json!({}), "viber@pc", "", None)?;
            c.execute("UPDATE approvals SET expires_at = 0 WHERE state = 'pending'", [])?;
            assert_eq!(super::poll(c, &poll)?.unwrap()["state"], "expired");
            for i in 0..MAX_PENDING {
                create(c, "connect", &json!({}), &format!("m{i}"), "", None)?;
            }
            assert!(create(c, "connect", &json!({}), "one more", "", None).is_err(), "a flood is cut off");
            Ok(())
        })
        .unwrap();
    }
}
