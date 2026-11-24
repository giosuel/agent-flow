-- Timings of builds and tests (wall, CPU, peak memory), on a task, a step or a PR (the owner,
-- 2026-11-12: they replace the timings in PR bodies).
CREATE TABLE timings (
    id           INTEGER PRIMARY KEY,
    project_id   INTEGER NOT NULL REFERENCES projects (id),
    record_id    INTEGER REFERENCES records (id),
    milestone_id INTEGER REFERENCES milestones (id),
    what         TEXT NOT NULL,
    place        TEXT NOT NULL DEFAULT 'here',
    wall_ms      INTEGER NOT NULL,
    cpu_ms       INTEGER,
    peak_mb      INTEGER,
    exit_code    INTEGER,
    note         TEXT NOT NULL DEFAULT '',
    actor        TEXT NOT NULL,
    at           INTEGER NOT NULL
);
CREATE INDEX timings_record ON timings (record_id);
CREATE INDEX timings_milestone ON timings (milestone_id);
