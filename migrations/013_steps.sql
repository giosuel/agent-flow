-- Steps start by themselves (a task goes to doing) and the agent concludes them with a summary
-- (the owner, 2026-11-12). "closed" is a concluded step.
ALTER TABLE milestones ADD COLUMN started_at INTEGER;
ALTER TABLE milestones ADD COLUMN summary TEXT NOT NULL DEFAULT '';
