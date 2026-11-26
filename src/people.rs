//! The people who sign in (the owner, 2026-11-25: D-270, D-272, D-273, D-274). The first is the
//! admin, made by `sudo flow server setup`; only the admin invites others, and everyone has the
//! same rights otherwise. Passkeys, browsers and push subscriptions belong to a person; projects
//! and records stay shared, and what each person does is recorded with their username.

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use webauthn_rs::prelude::Uuid;

use crate::util::now_ms;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Person {
    pub id: i64,
    /// Picked at the first sign-in; None until then.
    pub username: Option<String>,
    pub email: String,
    /// One of the profile icons by name, or "".
    pub icon: String,
    pub admin: bool,
    pub invited_by: Option<i64>,
    pub created_at: i64,
}

impl Person {
    /// How the person is named on pages and in History: their username, or a stand-in until
    /// they've picked one.
    pub fn name(&self) -> String {
        match &self.username {
            Some(u) => u.clone(),
            None if self.admin => "the admin".into(),
            None => format!("person {}", self.id),
        }
    }

    /// Their username, email and icon are set (the first sign-in's step 2).
    pub fn has_profile(&self) -> bool {
        self.username.is_some() && !self.icon.is_empty()
    }
}

const COLUMNS: &str = "id, username, email, icon, admin, invited_by, created_at";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Person> {
    Ok(Person {
        id: r.get(0)?,
        username: r.get(1)?,
        email: r.get(2)?,
        icon: r.get(3)?,
        admin: r.get::<_, i64>(4)? != 0,
        invited_by: r.get(5)?,
        created_at: r.get(6)?,
    })
}

/// A person who hasn't been removed.
pub fn get(conn: &Connection, id: i64) -> anyhow::Result<Option<Person>> {
    Ok(conn
        .query_row(&format!("SELECT {COLUMNS} FROM people WHERE id = ?1 AND removed_at IS NULL"), [id], row)
        .optional()?)
}

pub fn by_username(conn: &Connection, username: &str) -> anyhow::Result<Option<Person>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM people WHERE username = ?1 COLLATE NOCASE AND removed_at IS NULL"),
            [username.trim()],
            row,
        )
        .optional()?)
}

/// Everyone on this AgentFlow, the admin first.
pub fn list(conn: &Connection) -> anyhow::Result<Vec<Person>> {
    let mut stmt =
        conn.prepare(&format!("SELECT {COLUMNS} FROM people WHERE removed_at IS NULL ORDER BY admin DESC, id"))?;
    let rows = stmt.query_map([], row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn admin(conn: &Connection) -> anyhow::Result<Option<Person>> {
    Ok(conn
        .query_row(&format!("SELECT {COLUMNS} FROM people WHERE admin = 1 AND removed_at IS NULL"), [], row)
        .optional()?)
}

/// Adds a person: the admin (only while there's none) or someone the admin invited.
pub fn create(conn: &Connection, admin: bool, invited_by: Option<i64>, webauthn_id: Uuid) -> anyhow::Result<i64> {
    if admin && self::admin(conn)?.is_some() {
        bail!("this AgentFlow already has an admin");
    }
    conn.execute(
        "INSERT INTO people (admin, webauthn_id, invited_by, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![admin as i64, webauthn_id.to_string(), invited_by, now_ms()],
    )?;
    Ok(conn.last_insert_rowid())
}

/// The person's WebAuthn user handle; made the first time it's needed.
pub fn webauthn_id(conn: &Connection, id: i64) -> anyhow::Result<Uuid> {
    let stored: String = conn
        .query_row("SELECT webauthn_id FROM people WHERE id = ?1", [id], |r| r.get(0))
        .optional()?
        .context("no such person")?;
    if let Ok(u) = Uuid::parse_str(&stored) {
        return Ok(u);
    }
    let u = Uuid::new_v4();
    conn.execute("UPDATE people SET webauthn_id = ?1 WHERE id = ?2", params![u.to_string(), id])?;
    Ok(u)
}

/// Setting AgentFlow up again when the admin lost their passkeys (D-274): the admin's passkeys go
/// and their browsers are signed out; their profile, projects, records and the other people stay.
/// Returns how many passkeys and browsers went.
pub fn reset_admin(conn: &Connection, admin_id: i64) -> anyhow::Result<(usize, usize)> {
    let keys = conn.execute("DELETE FROM passkeys WHERE person_id = ?1", [admin_id])?;
    let browsers = conn.execute(
        "UPDATE web_sessions SET revoked_at = ?1 WHERE person_id = ?2 AND revoked_at IS NULL",
        params![now_ms(), admin_id],
    )?;
    Ok((keys, browsers))
}

/// How many passkeys and signed-in browsers a person has, for the setup command's warning.
pub fn counts(conn: &Connection, id: i64) -> anyhow::Result<(i64, i64)> {
    let keys = conn.query_row("SELECT count(*) FROM passkeys WHERE person_id = ?1", [id], |r| r.get(0))?;
    let browsers = conn.query_row(
        "SELECT count(*) FROM web_sessions WHERE person_id = ?1 AND revoked_at IS NULL AND expires_at > ?2",
        params![id, now_ms()],
        |r| r.get(0),
    )?;
    Ok((keys, browsers))
}

/// Names a person can't take: words pages use for someone else.
const RESERVED: &[&str] =
    &["owner", "you", "admin", "system", "github", "agentflow", "consultant", "anonymous", "root", "flow", "agent"];

/// A username as kept: lowercase letters, digits and dashes, 2 to 32 of them, starting and ending
/// with a letter or digit.
pub fn check_username(raw: &str) -> anyhow::Result<String> {
    let u = raw.trim().to_lowercase();
    if !(2..=32).contains(&u.chars().count()) {
        bail!("a username has 2 to 32 characters");
    }
    if !u.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        bail!("a username has only lowercase letters, digits and dashes");
    }
    if u.starts_with('-') || u.ends_with('-') {
        bail!("a username starts and ends with a letter or digit");
    }
    if RESERVED.contains(&u.as_str()) {
        bail!("{u:?} is taken by AgentFlow itself; pick another");
    }
    Ok(u)
}

/// An email address's shape: something@something.something, nothing more is checked.
pub fn check_email(raw: &str) -> anyhow::Result<String> {
    let e = raw.trim();
    let ok = e.len() <= 200
        && !e.chars().any(char::is_whitespace)
        && e.split_once('@').is_some_and(|(a, b)| {
            !a.is_empty() && !b.contains('@') && b.contains('.') && !b.starts_with('.') && !b.ends_with('.')
        });
    if !ok {
        bail!("that doesn't look like an email address");
    }
    Ok(e.to_owned())
}

/// Sets a person's username, email and icon (the first sign-in, Settings › Profile).
pub fn set_profile(conn: &Connection, id: i64, username: &str, email: &str, icon: &str) -> anyhow::Result<()> {
    let username = check_username(username)?;
    let email = check_email(email)?;
    if !crate::web::avatars::is_avatar(icon) {
        bail!("pick one of the icons");
    }
    // A project's agent signs with the project's slug: a person of the same name would be mistaken
    // for it.
    let project: Option<i64> = conn
        .query_row("SELECT id FROM projects WHERE slug = ?1 COLLATE NOCASE", [&username], |r| r.get(0))
        .optional()?;
    if project.is_some() {
        bail!("{username:?} is a project's name; pick another");
    }
    if by_username(conn, &username)?.is_some_and(|p| p.id != id) {
        bail!("someone here is already called {username:?}");
    }
    conn.execute(
        "UPDATE people SET username = ?1, email = ?2, icon = ?3 WHERE id = ?4 AND removed_at IS NULL",
        params![username, email, icon, id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn one_admin_and_the_people_they_invite() {
        let db = Db::open_memory();
        db.call_blocking(|c| {
            assert!(admin(c)?.is_none());
            let a = create(c, true, None, Uuid::new_v4())?;
            assert!(create(c, true, None, Uuid::new_v4()).is_err(), "only one admin");
            let m = create(c, false, Some(a), Uuid::new_v4())?;
            let everyone = list(c)?;
            assert_eq!(everyone.iter().map(|p| p.id).collect::<Vec<_>>(), vec![a, m], "the admin first");
            assert_eq!(get(c, a)?.unwrap().name(), "the admin");
            assert_eq!(get(c, m)?.unwrap().name(), format!("person {m}"));
            assert!(!get(c, m)?.unwrap().has_profile());
            let u = webauthn_id(c, a)?;
            assert_eq!(webauthn_id(c, a)?, u, "the same handle every time");
            // Profiles: a username of letters, digits and dashes, a plausible email, one of the icons.
            assert!(set_profile(c, a, "Gio Suel", "me@giosuel.ch", "owl").is_err());
            assert!(set_profile(c, a, "you", "me@giosuel.ch", "owl").is_err());
            assert!(set_profile(c, a, "giosuel", "me", "owl").is_err());
            assert!(set_profile(c, a, "giosuel", "me@giosuel.ch", "dragon").is_err());
            set_profile(c, a, "GioSuel", "me@giosuel.ch", "owl")?;
            assert_eq!(get(c, a)?.unwrap().username.as_deref(), Some("giosuel"));
            assert!(get(c, a)?.unwrap().has_profile());
            assert!(set_profile(c, m, "giosuel", "m@x.org", "fox").is_err(), "taken");
            c.execute("INSERT INTO projects (slug, name, created_at) VALUES ('lituus', 'Lituus', 0)", [])?;
            assert!(set_profile(c, m, "lituus", "m@x.org", "fox").is_err(), "a project's name");
            set_profile(c, m, "mira", "m@x.org", "fox")?;
            Ok(())
        })
        .unwrap();
    }
}
