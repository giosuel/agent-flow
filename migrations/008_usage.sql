-- Token usage, read from Claude Code's transcripts (which it deletes after 30 days, so the
-- counts are kept here). One row per model call; message ids are unique across files, since a
-- subagent's transcript can repeat its parent's.
CREATE TABLE usage (
    message_id  TEXT PRIMARY KEY,
    at          INTEGER NOT NULL,
    -- The transcript folder, e.g. "-home-viber-office": the session's starting directory.
    folder      TEXT NOT NULL,
    -- Where the session was working at that call; decides the project.
    cwd         TEXT NOT NULL DEFAULT '',
    session     TEXT NOT NULL,
    subagent    INTEGER NOT NULL DEFAULT 0,
    model       TEXT NOT NULL,
    project_id  INTEGER REFERENCES projects (id),
    task_id     INTEGER REFERENCES records (id),
    input       INTEGER NOT NULL,
    output      INTEGER NOT NULL,
    cache_write INTEGER NOT NULL,
    cache_read  INTEGER NOT NULL
);
CREATE INDEX usage_at ON usage (at);
CREATE INDEX usage_project ON usage (project_id, at);
CREATE INDEX usage_task ON usage (task_id);
CREATE INDEX usage_session ON usage (session);

-- How far each transcript has been read.
CREATE TABLE usage_files (
    path     TEXT PRIMARY KEY,
    offset   INTEGER NOT NULL,
    -- For a subagent, its first own call's time: its tokens go to the task its session was on then.
    first_at INTEGER
);

-- The machine every minute, per group: a project's slug (its agent with everything it
-- started), 'other' for the rest, and 'machine' for the whole machine.
CREATE TABLE machine_samples (
    at         INTEGER NOT NULL,
    grp        TEXT NOT NULL,
    cores      REAL NOT NULL,
    mem_kb     INTEGER NOT NULL,
    top        TEXT NOT NULL DEFAULT ''
);
CREATE INDEX machine_samples_at ON machine_samples (at);
