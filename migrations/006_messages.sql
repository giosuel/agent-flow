-- M6: conversations between agents, the consultant and the owner. Claude Code's own
-- SendMessage can't be logged, so agents talk through AgentFlow.
CREATE TABLE threads (
    id         INTEGER PRIMARY KEY,
    subject    TEXT NOT NULL,
    started_by TEXT NOT NULL,       -- a project slug, "consultant" or "owner"
    created_at INTEGER NOT NULL,
    last_at    INTEGER NOT NULL
);

-- Who takes part in a thread: project slugs, "consultant", or "all".
CREATE TABLE thread_members (
    thread_id INTEGER NOT NULL REFERENCES threads (id),
    member    TEXT NOT NULL,
    PRIMARY KEY (thread_id, member)
);

CREATE TABLE messages (
    id         INTEGER PRIMARY KEY,
    thread_id  INTEGER NOT NULL REFERENCES threads (id),
    sender     TEXT NOT NULL,       -- a project slug, "consultant" or "owner"
    kind       TEXT NOT NULL CHECK (kind IN ('info', 'question', 'answer', 'resource', 'handover')),
    body       TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX messages_thread ON messages (thread_id, id);

-- How far the consultant (who has no project) has read its messages.
CREATE TABLE consultant_reads (
    reader    TEXT PRIMARY KEY,
    last_id   INTEGER NOT NULL
);
