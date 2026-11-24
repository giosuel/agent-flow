-- The session log (the owner, 2026-11-12: "keep the claude code sendmessage and use agent flow
-- only for logging for this"): what sessions and their subagents tell each other, read from
-- Claude Code's transcripts. AgentFlow's own messages (threads, messages) are no longer written.
CREATE TABLE session_messages (
    id         INTEGER PRIMARY KEY,
    key        TEXT NOT NULL UNIQUE,      -- the conversation, the sender and the text: seen from both sides, kept once
    at         INTEGER NOT NULL,
    thread     TEXT NOT NULL,             -- "pair:<a>|<b>" (sessions' names, sorted) or "sub:<agent id>"
    from_name  TEXT NOT NULL,
    to_name    TEXT NOT NULL,
    body       TEXT NOT NULL,
    folder     TEXT NOT NULL DEFAULT '',
    cwd        TEXT NOT NULL DEFAULT '',
    session    TEXT NOT NULL DEFAULT '',
    project_id INTEGER REFERENCES projects (id)
);
CREATE INDEX session_messages_thread ON session_messages (thread, at);

CREATE TABLE subagent_threads (
    agent_id    TEXT PRIMARY KEY,
    session     TEXT NOT NULL,             -- its parent session
    folder      TEXT NOT NULL,
    cwd         TEXT NOT NULL DEFAULT '',
    description TEXT NOT NULL DEFAULT '',
    agent_type  TEXT NOT NULL DEFAULT '',
    project_id  INTEGER REFERENCES projects (id),
    started_at  INTEGER NOT NULL,
    last_at     INTEGER NOT NULL
);

-- Sessions' own names (/rename) and the addresses other sessions reach them at.
CREATE TABLE session_names (session TEXT PRIMARY KEY, name TEXT NOT NULL, folder TEXT NOT NULL, at INTEGER NOT NULL);
CREATE TABLE session_addresses (address TEXT PRIMARY KEY, name TEXT NOT NULL, at INTEGER NOT NULL);

-- Read every transcript once more from the start for the messages in what was read before
-- (model calls are keyed by their id, so none counts twice).
UPDATE usage_files SET offset = 0;
