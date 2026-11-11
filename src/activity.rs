//! The activity log: one row for everything that happens, written as it happens. The Log page (M8)
//! reads it back; nothing is ever deleted.

use rusqlite::{Connection, params};
use serde_json::Value;

use crate::auth::Actor;
use crate::db::Db;
use crate::util::now_ms;

/// How the action reached AgentFlow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Page,
    Api,
    Mcp,
    Cli,
    Button,
    Poller,
    System,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Via::Page => "page",
            Via::Api => "api",
            Via::Mcp => "mcp",
            Via::Cli => "cli",
            Via::Button => "button",
            Via::Poller => "poller",
            Via::System => "system",
        }
    }
}

/// What kind of thing happened; the Log page filters on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Read,
    Write,
    Run,
    Login,
    Push,
    /// A refused request: bad token, missing session, not allowed.
    Denied,
    System,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Read => "read",
            Action::Write => "write",
            Action::Run => "run",
            Action::Login => "login",
            Action::Push => "push",
            Action::Denied => "denied",
            Action::System => "system",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub actor_kind: String,
    pub actor: String,
    pub project: Option<String>,
    pub via: Via,
    pub action: Action,
    pub summary: String,
    pub target: Option<String>,
    pub status: Option<i64>,
    pub duration_ms: Option<i64>,
    pub ip: Option<String>,
    pub detail: Option<Value>,
    pub severity: &'static str,
}

impl Entry {
    pub fn new(actor: &Actor, via: Via, action: Action, summary: impl Into<String>) -> Self {
        Self {
            actor_kind: actor.kind().into(),
            actor: actor.name(),
            project: actor.project().map(Into::into),
            via,
            action,
            summary: clip(&redact(&summary.into()), 300),
            target: None,
            status: None,
            duration_ms: None,
            ip: None,
            detail: None,
            severity: if action == Action::Denied { "warn" } else { "info" },
        }
    }

    pub fn project(mut self, slug: impl Into<String>) -> Self {
        self.project = Some(slug.into());
        self
    }

    pub fn target(mut self, id: impl Into<String>) -> Self {
        self.target = Some(id.into());
        self
    }

    pub fn status(mut self, status: u16) -> Self {
        self.status = Some(status.into());
        self
    }

    pub fn duration_ms(mut self, ms: i64) -> Self {
        self.duration_ms = Some(ms);
        self
    }

    pub fn ip(mut self, ip: Option<String>) -> Self {
        self.ip = ip;
        self
    }

    pub fn severity(mut self, severity: &'static str) -> Self {
        self.severity = severity;
        self
    }
}

pub fn write_conn(conn: &Connection, e: &Entry) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO log (at, actor_kind, actor, project, via, action, summary, target, status, duration_ms, ip, detail, severity)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            now_ms(),
            e.actor_kind,
            e.actor,
            e.project,
            e.via.as_str(),
            e.action.as_str(),
            e.summary,
            e.target,
            e.status,
            e.duration_ms,
            e.ip,
            e.detail.as_ref().map(|d| d.to_string()),
            e.severity,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Writes an entry. A failure to log is reported but never fails the request it describes.
pub async fn write(db: &Db, e: Entry) {
    if let Err(err) = db.call(move |c| Ok(write_conn(c, &e)?)).await {
        tracing::error!("could not write to the activity log: {err:#}");
    }
}

/// Cuts text to `max` characters, marking the cut. Keeps one row from being huge (an anonymous
/// request with a 20 KB URL would otherwise be stored whole, for good).
pub fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_owned(),
    }
}

/// Prefixes of secrets that must never reach the log: AgentFlow's own tokens and GitHub's.
const SECRET_PREFIXES: &[&str] = &["af_", "ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"];

/// Replaces anything that looks like a token with `[redacted]`.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    'outer: while !rest.is_empty() {
        for prefix in SECRET_PREFIXES {
            if rest.starts_with(prefix) && at_word_start(text, rest) {
                let len =
                    rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')).unwrap_or(rest.len());
                if len >= prefix.len() + 16 {
                    out.push_str("[redacted]");
                    rest = &rest[len..];
                    continue 'outer;
                }
            }
        }
        let c = rest.chars().next().unwrap();
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

fn at_word_start(whole: &str, rest: &str) -> bool {
    let i = whole.len() - rest.len();
    i == 0 || !whole[..i].chars().next_back().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_redacted() {
        let t = crate::util::new_secret("af_prj_");
        assert_eq!(redact(&format!("Bearer {t} done")), "Bearer [redacted] done");
        assert_eq!(redact("ghp_abcdefghijklmnopqrstuvwxyz0123"), "[redacted]");
        // Short or embedded look-alikes stay.
        assert_eq!(redact("leaf_node af_x"), "leaf_node af_x");
        assert_eq!(redact("grüße af_"), "grüße af_");
    }
}
