-- A review is a step of its own (the owner, 2026-11-12): who reviewed which commit, the owner's
-- words, the reports. Its findings are the step's tasks.
ALTER TABLE milestones ADD COLUMN review TEXT NOT NULL DEFAULT '';  -- JSON, empty for other steps
