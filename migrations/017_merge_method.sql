-- How the project's PRs merge (Imaginarium's agent, 2026-11-12: merge commits, never squash);
-- every PR of its repository gets its Merge button with it, so agents don't write them by hand.
ALTER TABLE projects ADD COLUMN merge_method TEXT NOT NULL DEFAULT 'merge';
