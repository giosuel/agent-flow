-- M3: how far each project's agent has read its event feed ("since my last check").
CREATE TABLE cursors (
    project_id INTEGER PRIMARY KEY REFERENCES projects (id),
    last_seq   INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
