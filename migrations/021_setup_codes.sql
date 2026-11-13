-- Setup codes (M11.3, the owner, 2026-11-13): the page makes one for a project ("Set up on another
-- machine", or a project created there without a machine yet); `flow project setup` on that machine
-- trades it once for the project's token. Valid 15 minutes, kept only as a hash.
CREATE TABLE setup_codes (
    id         INTEGER PRIMARY KEY,
    hash       BLOB NOT NULL UNIQUE,
    project_id INTEGER NOT NULL REFERENCES projects (id),
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at    INTEGER,
    used_by    TEXT
);
