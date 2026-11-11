-- M0: projects (just enough for project tokens), access, and the activity log.
-- Times are milliseconds since the Unix epoch, UTC.

CREATE TABLE projects (
    id         INTEGER PRIMARY KEY,
    slug       TEXT NOT NULL UNIQUE,
    name       TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

-- API tokens: a device of the owner's (CLI, viber-run), one project's agent, or the consultant (admin).
-- Only the SHA-256 of the token is stored; the token itself is shown once, when it's created.
CREATE TABLE tokens (
    id             INTEGER PRIMARY KEY,
    kind           TEXT NOT NULL CHECK (kind IN ('device', 'project', 'admin')),
    name           TEXT NOT NULL,
    project_id     INTEGER REFERENCES projects (id),
    hash           BLOB NOT NULL UNIQUE,
    created_at     INTEGER NOT NULL,
    last_used_at   INTEGER,
    last_used_from TEXT,
    revoked_at     INTEGER,
    CHECK ((kind = 'project') = (project_id IS NOT NULL))
);

-- One-time links printed by `agentflow admin login-link` (sign-in on a new device, passkey enrolment).
CREATE TABLE login_links (
    hash       BLOB PRIMARY KEY,
    purpose    TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at    INTEGER
);

-- The owner's browser sessions (cookie), however they signed in.
CREATE TABLE web_sessions (
    id           INTEGER PRIMARY KEY,
    hash         BLOB NOT NULL UNIQUE,
    method       TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL,
    user_agent   TEXT,
    ip           TEXT,
    revoked_at   INTEGER
);

-- Everything that happens: every request, tool call, message, button press and background job.
-- Rows are never changed or deleted; old reads move to compressed archives (M8).
CREATE TABLE log (
    seq         INTEGER PRIMARY KEY,
    at          INTEGER NOT NULL,
    actor_kind  TEXT NOT NULL,
    actor       TEXT NOT NULL,
    project     TEXT,
    via         TEXT NOT NULL,
    action      TEXT NOT NULL,
    summary     TEXT NOT NULL,
    target      TEXT,
    status      INTEGER,
    duration_ms INTEGER,
    ip          TEXT,
    detail      TEXT,
    severity    TEXT NOT NULL DEFAULT 'info'
);
CREATE INDEX log_at ON log (at);
CREATE INDEX log_project_at ON log (project, at);
CREATE INDEX log_target ON log (target);
