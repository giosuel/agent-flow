-- A builder's token (`af_bld_…`) may only connect as that builder. SQLite can't change a CHECK, so
-- the table is rebuilt with the new kind; nothing else changes and no row is lost.
CREATE TABLE tokens_new (
    id             INTEGER PRIMARY KEY,
    kind           TEXT NOT NULL CHECK (kind IN ('device', 'project', 'admin', 'builder')),
    name           TEXT NOT NULL,
    project_id     INTEGER REFERENCES projects (id),
    hash           BLOB NOT NULL UNIQUE,
    created_at     INTEGER NOT NULL,
    last_used_at   INTEGER,
    last_used_from TEXT,
    revoked_at     INTEGER,
    hint           TEXT NOT NULL DEFAULT '',
    CHECK ((kind = 'project') = (project_id IS NOT NULL))
);
INSERT INTO tokens_new (id, kind, name, project_id, hash, created_at, last_used_at, last_used_from, revoked_at, hint)
    SELECT id, kind, name, project_id, hash, created_at, last_used_at, last_used_from, revoked_at, hint FROM tokens;
DROP TABLE tokens;
ALTER TABLE tokens_new RENAME TO tokens;
