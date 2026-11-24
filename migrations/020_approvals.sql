-- Approvals (M11.2, the owner, 2026-11-13): `flow connect` and the owner's admin jobs from `flow`
-- (tokens, projects, backups) wait until the owner approves them on the page, signed in with their
-- passkey. `code` is what both sides show; flow polls with a secret of its own (kept hashed). A
-- result (e.g. a new token) is handed to flow once and then cleared.
CREATE TABLE approvals (
    id           INTEGER PRIMARY KEY,
    code         TEXT NOT NULL UNIQUE,
    poll_hash    TEXT NOT NULL UNIQUE,
    action       TEXT NOT NULL,            -- connect, token_create, token_revoke, token_rotate, project_add, backup, import_prs
    args         TEXT NOT NULL DEFAULT '{}',
    machine      TEXT NOT NULL DEFAULT '', -- "user@host" as flow reports it
    requested_by TEXT NOT NULL DEFAULT '', -- the token's name, or "" for a connect without one
    ip           TEXT,
    state        TEXT NOT NULL DEFAULT 'pending', -- pending, approved, denied, done, expired, failed
    result       TEXT,
    error        TEXT,
    created_at   INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL,
    decided_at   INTEGER,
    fetched_at   INTEGER
);
CREATE INDEX approvals_state ON approvals (state, created_at);
