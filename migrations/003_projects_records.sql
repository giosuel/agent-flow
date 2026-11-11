-- M2: project details, the project's agent, milestones, and typed records with history.

ALTER TABLE projects ADD COLUMN description TEXT NOT NULL DEFAULT '';
ALTER TABLE projects ADD COLUMN status TEXT NOT NULL DEFAULT 'active';
ALTER TABLE projects ADD COLUMN repo TEXT NOT NULL DEFAULT '';          -- e.g. giosuel/vinculum
ALTER TABLE projects ADD COLUMN checkout TEXT NOT NULL DEFAULT '';      -- e.g. ~/vinculum
ALTER TABLE projects ADD COLUMN machine TEXT NOT NULL DEFAULT '';
ALTER TABLE projects ADD COLUMN stack TEXT NOT NULL DEFAULT '[]';       -- JSON list
ALTER TABLE projects ADD COLUMN platform TEXT NOT NULL DEFAULT '';
ALTER TABLE projects ADD COLUMN model TEXT NOT NULL DEFAULT '';
ALTER TABLE projects ADD COLUMN effort TEXT NOT NULL DEFAULT '';
ALTER TABLE projects ADD COLUMN pm_rules TEXT NOT NULL DEFAULT '[]';    -- JSON list, override the defaults
ALTER TABLE projects ADD COLUMN git_rules TEXT NOT NULL DEFAULT '';     -- e.g. "DCO sign-off"
ALTER TABLE projects ADD COLUMN ports TEXT NOT NULL DEFAULT '[]';       -- JSON list
ALTER TABLE projects ADD COLUMN build_product TEXT NOT NULL DEFAULT ''; -- viber-run product name
ALTER TABLE projects ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0;
-- The project's agent, as it last introduced itself.
ALTER TABLE projects ADD COLUMN agent_session TEXT NOT NULL DEFAULT '';
ALTER TABLE projects ADD COLUMN agent_machine TEXT NOT NULL DEFAULT '';
ALTER TABLE projects ADD COLUMN agent_model TEXT NOT NULL DEFAULT '';
ALTER TABLE projects ADD COLUMN agent_effort TEXT NOT NULL DEFAULT '';
ALTER TABLE projects ADD COLUMN agent_seen_at INTEGER;
ALTER TABLE projects ADD COLUMN agent_note TEXT NOT NULL DEFAULT '';

-- Milestones (M3, A3, P4 or any name) and their steps (parent_id set, e.g. K1–K4 inside M4).
CREATE TABLE milestones (
    id               INTEGER PRIMARY KEY,
    project_id       INTEGER NOT NULL REFERENCES projects (id),
    parent_id        INTEGER REFERENCES milestones (id),
    name             TEXT NOT NULL,
    title            TEXT NOT NULL,
    description      TEXT NOT NULL DEFAULT '',
    position         REAL NOT NULL,
    state            TEXT NOT NULL DEFAULT 'planned' CHECK (state IN ('planned', 'active', 'closed')),
    tag              TEXT NOT NULL DEFAULT '',
    reopened_because TEXT NOT NULL DEFAULT '',
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    closed_at        INTEGER
);
CREATE UNIQUE INDEX milestones_name ON milestones (project_id, coalesce(parent_id, 0), name);

-- Every typed record: question, decision, task, ADR (and later PRs, builds, feedback…).
-- Shown as <project>/<prefix>-<number>, e.g. vinculum/Q-12; numbers are never reused.
CREATE TABLE records (
    id           INTEGER PRIMARY KEY,
    project_id   INTEGER NOT NULL REFERENCES projects (id),
    kind         TEXT NOT NULL,
    number       INTEGER NOT NULL,
    title        TEXT NOT NULL,
    body         TEXT NOT NULL DEFAULT '',
    state        TEXT NOT NULL,
    milestone_id INTEGER REFERENCES milestones (id),
    position     REAL,
    tags         TEXT NOT NULL DEFAULT '',   -- ",git,pull-requests," so LIKE '%,git,%' finds a tag
    data         TEXT NOT NULL DEFAULT '{}', -- the kind's own fields, JSON
    author       TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    UNIQUE (project_id, kind, number)
);
CREATE INDEX records_kind_state ON records (project_id, kind, state);
CREATE INDEX records_milestone ON records (milestone_id);

-- Records are never edited in place without a trace: every change appends a revision.
CREATE TABLE revisions (
    id        INTEGER PRIMARY KEY,
    record_id INTEGER NOT NULL REFERENCES records (id),
    rev       INTEGER NOT NULL,
    at        INTEGER NOT NULL,
    author    TEXT NOT NULL,
    changes   TEXT NOT NULL,   -- JSON: the fields that changed, with their new values
    note      TEXT NOT NULL DEFAULT '',
    UNIQUE (record_id, rev)
);

-- Typed edges between records: supersedes, answers, blocks, implements, records, depends_on, …
CREATE TABLE links (
    id         INTEGER PRIMARY KEY,
    from_id    INTEGER NOT NULL REFERENCES records (id),
    to_id      INTEGER NOT NULL REFERENCES records (id),
    relation   TEXT NOT NULL,
    author     TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (from_id, to_id, relation)
);
CREATE INDEX links_to ON links (to_id);

-- What happened, per project, for agents ("since my last check") and the dashboard's activity.
CREATE TABLE events (
    seq        INTEGER PRIMARY KEY,
    at         INTEGER NOT NULL,
    project_id INTEGER REFERENCES projects (id),
    record_id  INTEGER REFERENCES records (id),
    kind       TEXT NOT NULL,
    summary    TEXT NOT NULL,
    actor      TEXT NOT NULL
);
CREATE INDEX events_project ON events (project_id, seq);
