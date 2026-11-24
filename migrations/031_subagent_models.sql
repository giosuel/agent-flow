-- The model a subagent ran on (the owner, 2026-11-19, D-228: "lets do model only"), and the notes
-- subagents uploaded with `flow note` (D-229): their call, from the subagent's transcript, until the
-- note it made is found and tagged.
ALTER TABLE subagent_threads ADD COLUMN model TEXT NOT NULL DEFAULT '';
CREATE TABLE subagent_note_calls (
    agent_id   TEXT NOT NULL,
    at         INTEGER NOT NULL,
    filename   TEXT NOT NULL,
    project_id INTEGER REFERENCES projects (id),
    done       INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (agent_id, at, filename)
);
