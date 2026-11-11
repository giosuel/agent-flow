-- Links to the agents' Claude Code sessions (remote control), read from their transcripts: the
-- project pages link to the newest one (the owner, 2026-11-11: "some global project wide button to
-- open the claude session").
CREATE TABLE remote_sessions (
    session TEXT NOT NULL,
    url TEXT NOT NULL,
    at INTEGER NOT NULL,
    folder TEXT NOT NULL,
    cwd TEXT NOT NULL,
    PRIMARY KEY (session, url)
);
CREATE INDEX remote_sessions_at ON remote_sessions (at);

-- Read every transcript once more to find the links in what was read before; calls are keyed by
-- their message id, so none counts twice.
UPDATE usage_files SET offset = 0;
