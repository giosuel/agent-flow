-- Builds and tests the agents run on a build server (the owner's workstation, over WireGuard):
-- the owner, 2026-11-11: "I want the agents to be able to use my much stronger workstation ... for
-- builds or tests if that machine is running ... I'd like to see the logs and monitor this on
-- agent-flow as well."
CREATE TABLE remote_jobs (
    id INTEGER PRIMARY KEY,
    server TEXT NOT NULL,
    project_id INTEGER NOT NULL REFERENCES projects (id),
    actor TEXT NOT NULL,
    -- The agent's checkout (its top level) and where in it the command runs.
    dir TEXT NOT NULL,
    subdir TEXT NOT NULL DEFAULT '',
    command TEXT NOT NULL,          -- JSON array of arguments
    fetch TEXT NOT NULL DEFAULT '[]', -- JSON array of paths to bring back
    gpu INTEGER NOT NULL DEFAULT 0,
    -- queued, running, paused, done, failed, cancelled
    state TEXT NOT NULL,
    reason TEXT NOT NULL DEFAULT '',
    exit_code INTEGER,
    commit_sha TEXT NOT NULL DEFAULT '',
    dirty INTEGER NOT NULL DEFAULT 0,
    threads INTEGER,
    sent_bytes INTEGER,
    lines INTEGER NOT NULL DEFAULT 0,
    warnings INTEGER NOT NULL DEFAULT 0,
    errors INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    started_at INTEGER,
    finished_at INTEGER
);
CREATE INDEX remote_jobs_state ON remote_jobs (state, server);
CREATE INDEX remote_jobs_project ON remote_jobs (project_id, id);
