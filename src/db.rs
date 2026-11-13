//! SQLite access. A handful of connections in WAL mode, used from blocking threads so a slow query
//! never stalls the async runtime. Writers queue on SQLite's own lock (busy timeout), which is
//! plenty for about 15 agents and one owner.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use rusqlite::Connection;
use tokio::sync::Semaphore;

/// Schema migrations, applied in order. `PRAGMA user_version` holds how many have run.
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/001_init.sql"),
    include_str!("../migrations/002_passkeys_push.sql"),
    include_str!("../migrations/003_projects_records.sql"),
    include_str!("../migrations/004_cursors.sql"),
    include_str!("../migrations/005_actions.sql"),
    include_str!("../migrations/006_messages.sql"),
    include_str!("../migrations/007_token_hint.sql"),
    include_str!("../migrations/008_usage.sql"),
    include_str!("../migrations/009_remote_sessions.sql"),
    include_str!("../migrations/010_remote_jobs.sql"),
    include_str!("../migrations/011_machine_totals.sql"),
    include_str!("../migrations/012_build_server_registry.sql"),
    include_str!("../migrations/013_steps.sql"),
    include_str!("../migrations/014_timings.sql"),
    include_str!("../migrations/015_review_steps.sql"),
    include_str!("../migrations/016_project_doc.sql"),
    include_str!("../migrations/017_merge_method.sql"),
    include_str!("../migrations/018_scopes_archive.sql"),
    include_str!("../migrations/019_session_log.sql"),
    include_str!("../migrations/020_approvals.sql"),
    include_str!("../migrations/021_setup_codes.sql"),
];

const POOL_SIZE: usize = 6;

#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    idle: Mutex<Vec<Connection>>,
    permits: Semaphore,
}

impl Db {
    /// Opens (and creates) the database and brings its schema up to date.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            restrict_permissions(dir);
        }
        let mut first = open_connection(path)?;
        migrate(&mut first)?;
        restrict_permissions(path);
        Ok(Self {
            inner: Arc::new(Inner {
                path: path.to_owned(),
                idle: Mutex::new(vec![first]),
                permits: Semaphore::new(POOL_SIZE),
            }),
        })
    }

    /// An in-memory database for tests. Shared cache, so every pooled connection sees the same data.
    #[cfg(test)]
    pub fn open_memory() -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let name = format!(
            "file:agentflow-test-{}-{}?mode=memory&cache=shared",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        );
        let mut first = open_connection(Path::new(&name)).unwrap();
        migrate(&mut first).unwrap();
        // Keep one connection open for the database's lifetime: a shared in-memory database
        // disappears when its last connection closes.
        let keep = open_connection(Path::new(&name)).unwrap();
        Self {
            inner: Arc::new(Inner {
                path: name.into(),
                idle: Mutex::new(vec![first, keep]),
                permits: Semaphore::new(POOL_SIZE),
            }),
        }
    }

    /// Runs `f` with a connection on a blocking thread.
    pub async fn call<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> anyhow::Result<T> + Send + 'static,
    {
        let _permit = self.inner.permits.acquire().await.context("database closed")?;
        let mut conn = self.take()?;
        let inner = self.inner.clone();
        let (conn, result) = tokio::task::spawn_blocking(move || {
            let r = f(&mut conn);
            (conn, r)
        })
        .await
        .context("database task panicked")?;
        inner.idle.lock().unwrap_or_else(|e| e.into_inner()).push(conn);
        result
    }

    /// The same for code that isn't async (the CLI's local commands).
    pub fn call_blocking<T>(&self, f: impl FnOnce(&mut Connection) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let mut conn = self.take()?;
        let r = f(&mut conn);
        self.inner.idle.lock().unwrap_or_else(|e| e.into_inner()).push(conn);
        r
    }

    fn take(&self) -> anyhow::Result<Connection> {
        let pooled = self.inner.idle.lock().unwrap_or_else(|e| e.into_inner()).pop();
        match pooled {
            Some(c) => Ok(c),
            None => open_connection(&self.inner.path),
        }
    }
}

fn open_connection(path: &Path) -> anyhow::Result<Connection> {
    let conn = if path.to_string_lossy().starts_with("file:") {
        Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
                | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?
    } else {
        Connection::open(path).with_context(|| format!("opening {}", path.display()))?
    };
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;",
    )?;
    Ok(conn)
}

fn migrate(conn: &mut Connection) -> anyhow::Result<()> {
    let done: usize = conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? as usize;
    if done > MIGRATIONS.len() {
        anyhow::bail!("the database is from a newer AgentFlow (schema {done}, this one knows {})", MIGRATIONS.len());
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(done) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql).with_context(|| format!("migration {}", i + 1))?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
        tx.commit()?;
    }
    Ok(())
}

/// The database holds token hashes and the whole history: only the owner's user may read it.
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if path.is_dir() { 0o700 } else { 0o600 };
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_apply_and_pool_shares_data() {
        let db = Db::open_memory();
        db.call(|c| {
            c.execute("INSERT INTO projects (slug, name, created_at) VALUES ('p', 'P', 0)", [])?;
            Ok(())
        })
        .await
        .unwrap();
        // Hold several connections at once so the pool opens new ones.
        let futs =
            (0..8).map(|_| {
                let db = db.clone();
                async move {
                    db.call(|c| Ok(c.query_row("SELECT count(*) FROM projects", [], |r| r.get::<_, i64>(0))?)).await
                }
            });
        for n in futures::future::join_all(futs).await {
            assert_eq!(n.unwrap(), 1);
        }
    }

    #[test]
    fn reopening_keeps_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("flow.db");
        Db::open(&path).unwrap();
        let db = Db::open(&path).unwrap();
        let v: i64 = db.call_blocking(|c| Ok(c.query_row("PRAGMA user_version", [], |r| r.get(0))?)).unwrap();
        assert_eq!(v as usize, MIGRATIONS.len());
    }
}
