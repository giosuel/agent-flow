-- The plan's usage limits as Claude Code's status line reports them (the owner, 2026-11-17, M13):
-- how much of the 5-hour and the weekly window is used, and when each resets. At most one sample
-- a minute, whichever session sends it: the limits are the account's.
CREATE TABLE plan_samples (
    at          INTEGER NOT NULL,
    five_pct    REAL,
    five_reset  INTEGER,
    week_pct    REAL,
    week_reset  INTEGER,
    project_id  INTEGER REFERENCES projects (id),
    machine     TEXT NOT NULL DEFAULT ''
);
CREATE INDEX plan_samples_at ON plan_samples (at);
