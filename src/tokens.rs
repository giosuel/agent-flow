//! Token files for Claude Code's `headersHelper`: each project's token (and the consultant's) is
//! written to `~/.config/agentflow/tokens/<name>.token`, mode 600, and read when the agent connects.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension};

use crate::auth::{self, TokenKind};

/// The file a project's or the consultant's token lives in.
pub fn token_path(dir: &Path, project: Option<&str>, name: &str) -> PathBuf {
    let stem = match project {
        Some(slug) => slug.to_owned(),
        None => file_stem(name),
    };
    dir.join(format!("{stem}.token"))
}

/// "Viber Consultant" → "viber-consultant"
fn file_stem(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_owned();
    if out.is_empty() { "token".into() } else { out }
}

/// Writes a token file atomically, readable only by this user.
pub fn write_token_file(path: &Path, secret: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = path.parent().context("token path has no folder")?;
    std::fs::create_dir_all(dir)?;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    let tmp = dir.join(format!(".{}.tmp", path.file_name().and_then(|n| n.to_str()).unwrap_or("token")));
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    writeln!(f, "{secret}")?;
    f.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Replaces a project or admin token with a new one, writes it to its file and revokes the old
/// one. Device tokens live on other machines, so they're revoked and re-created by hand instead.
pub fn rotate(conn: &mut Connection, id: i64, dir: &Path) -> anyhow::Result<(i64, PathBuf)> {
    let tx = conn.transaction()?;
    let row = tx
        .query_row(
            "SELECT t.kind, t.name, p.slug FROM tokens t LEFT JOIN projects p ON p.id = t.project_id
             WHERE t.id = ?1 AND t.revoked_at IS NULL",
            [id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?)),
        )
        .optional()?;
    let Some((kind, name, project)) = row else { bail!("no active token {id}") };
    let kind: TokenKind = kind.parse()?;
    if kind == TokenKind::Device {
        bail!("device tokens can't be rotated here; revoke it and add a new one");
    }
    let (new_id, secret) = auth::create_token(&tx, kind, &name, project.as_deref())?;
    auth::revoke_token(&tx, id)?;
    let path = token_path(dir, project.as_deref(), &name);
    // Write the file before committing: if writing fails, the old token stays valid.
    write_token_file(&path, &secret)?;
    tx.commit()?;
    Ok((new_id, path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn rotate_writes_the_file_and_revokes() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_memory();
        db.call_blocking(|c| {
            c.execute("INSERT INTO projects (slug, name, created_at) VALUES ('vinculum', 'Vinculum', 0)", [])?;
            let (old, old_secret) = auth::create_token(c, TokenKind::Project, "Vinculum agent", Some("vinculum"))?;
            let (new, path) = rotate(c, old, dir.path())?;
            assert_ne!(old, new);
            assert_eq!(path, dir.path().join("vinculum.token"));
            let secret = std::fs::read_to_string(&path)?.trim().to_owned();
            assert!(auth::token_actor(c, &old_secret, "x")?.is_none());
            assert_eq!(auth::token_actor(c, &secret, "x")?.unwrap().project(), Some("vinculum"));
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);

            let (dev, _) = auth::create_token(c, TokenKind::Device, "laptop", None)?;
            assert!(rotate(c, dev, dir.path()).is_err());
            let (adm, _) = auth::create_token(c, TokenKind::Admin, "Viber Consultant", None)?;
            assert_eq!(rotate(c, adm, dir.path())?.1, dir.path().join("viber-consultant.token"));
            Ok(())
        })
        .unwrap();
    }
}
