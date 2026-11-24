-- The handbook (the owner, 2026-11-18: guides renamed; "lets actually make it so its not in git,
-- we can just have it as actual files, and then have the backup make sure we don't lose
-- anything"). Every file of it, kept here so the daily backup holds it, with each edit as a
-- revision; AgentFlow writes them out as files under <data_dir>/handbook and on every machine.
CREATE TABLE handbook_files (
    path       TEXT PRIMARY KEY,          -- "MACHINE.md", "templates/external-watch/README.md"
    text       TEXT NOT NULL,             -- a text file's content; '' for a binary file
    bin        BLOB,                      -- a binary file's bytes (images), else NULL
    rev        INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    author     TEXT NOT NULL,
    what       TEXT NOT NULL DEFAULT ''
);
CREATE TABLE handbook_revs (
    id     INTEGER PRIMARY KEY,
    path   TEXT NOT NULL,
    rev    INTEGER NOT NULL,
    at     INTEGER NOT NULL,
    author TEXT NOT NULL,
    what   TEXT NOT NULL DEFAULT '',
    text   TEXT NOT NULL,                -- empty (and bin NULL): the file was removed
    bin    BLOB
);
CREATE INDEX handbook_revs_path ON handbook_revs (path, rev);
