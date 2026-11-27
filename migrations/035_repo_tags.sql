-- A project repository's tags, newest first, as GitHub had them at the last poll: the Pull requests
-- tab lists them beside the next release (the owner, 2026-11-26, Q-37: "and also the past tags in
-- the repo?"). The title is the first line of an annotated tag's message, '' for a lightweight tag.
CREATE TABLE repo_tags (
    project_id INTEGER NOT NULL REFERENCES projects (id),
    name       TEXT NOT NULL,
    at         INTEGER NOT NULL,
    title      TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (project_id, name)
);
