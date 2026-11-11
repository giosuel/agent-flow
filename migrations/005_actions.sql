-- M4: buttons. An agent attaches the exact commands to a PR or a release; the owner's press is
-- the approval and runs them on the server. Actions never change after they're added: the page
-- shows the command, and the press must carry the hash of what was shown.
CREATE TABLE actions (
    id          INTEGER PRIMARY KEY,
    record_id   INTEGER NOT NULL REFERENCES records (id),
    label       TEXT NOT NULL,              -- "Merge", "Push tag v0.0.5-m4"
    command     TEXT NOT NULL,              -- run with bash -c
    workdir     TEXT NOT NULL DEFAULT '',   -- e.g. ~/vinculum; home if empty
    requires    TEXT NOT NULL DEFAULT '[]', -- JSON: record ids that must be merged or done first
    added_by    TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    state       TEXT NOT NULL DEFAULT 'waiting' CHECK (state IN ('waiting', 'running', 'done', 'failed', 'withdrawn')),
    pressed_at  INTEGER,
    finished_at INTEGER,
    exit_code   INTEGER,
    output      TEXT NOT NULL DEFAULT ''
);
CREATE INDEX actions_record ON actions (record_id);
