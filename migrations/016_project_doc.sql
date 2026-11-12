-- One Markdown document per project, the agent's to shape (the owner, 2026-11-12: "a generic
-- project overview document per project that the agent can basically do whatever with"). It takes
-- over the free-text details: tech stack, platform, git and PM rules ("everything project specific
-- except the structured metadata that every project has"). Every edit is a revision.
CREATE TABLE project_docs (
    project_id INTEGER PRIMARY KEY REFERENCES projects (id),
    text       TEXT NOT NULL,
    rev        INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    author     TEXT NOT NULL,
    what       TEXT NOT NULL DEFAULT ''
);
CREATE TABLE project_doc_revs (
    id         INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects (id),
    rev        INTEGER NOT NULL,
    at         INTEGER NOT NULL,
    author     TEXT NOT NULL,
    what       TEXT NOT NULL,
    text       TEXT NOT NULL,
    UNIQUE (project_id, rev)
);
WITH parts AS (
    SELECT id,
        CASE WHEN stack NOT IN ('', '[]') THEN '## Tech stack' || char(10) || char(10)
            || (SELECT group_concat(value, ' · ') FROM json_each(projects.stack)) || char(10) || char(10) ELSE '' END AS a,
        CASE WHEN platform != '' THEN '## Platform' || char(10) || char(10) || platform || char(10) || char(10) ELSE '' END AS b,
        CASE WHEN git_rules != '' THEN '## Git' || char(10) || char(10) || git_rules || char(10) || char(10) ELSE '' END AS c,
        CASE WHEN pm_rules NOT IN ('', '[]') THEN '## How to work on it' || char(10) || char(10)
            || (SELECT group_concat('- ' || value, char(10)) FROM json_each(projects.pm_rules)) || char(10) || char(10) ELSE '' END AS d
    FROM projects
)
INSERT INTO project_docs (project_id, text, rev, updated_at, author, what)
SELECT id, rtrim(a || b || c || d, char(10)) || char(10), 1, CAST(strftime('%s', 'now') AS INTEGER) * 1000, 'agentflow',
       'moved from the project details'
FROM parts WHERE a || b || c || d != '';
INSERT INTO project_doc_revs (project_id, rev, at, author, what, text)
SELECT project_id, rev, updated_at, author, what, text FROM project_docs;
