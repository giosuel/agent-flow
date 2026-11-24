-- Scopes and archiving (the owner, 2026-11-12): agents add, edit and archive milestones, steps,
-- tasks, decisions and questions freely; nothing is deleted. An archived milestone or step is
-- hidden from the lists, reachable by id and with include_archived, and can be restored.
ALTER TABLE milestones ADD COLUMN archived_at INTEGER;
ALTER TABLE milestones ADD COLUMN archived_reason TEXT NOT NULL DEFAULT '';
-- Archived along with this milestone (its steps), so restoring it brings them back.
ALTER TABLE milestones ADD COLUMN archived_with INTEGER REFERENCES milestones (id);

-- A decision's or question's scope is its milestone_id: a milestone (never a step), or NULL for
-- project-wide. Steps resolve to their milestone.
UPDATE records SET milestone_id = (SELECT coalesce(m.parent_id, m.id) FROM milestones m WHERE m.id = records.milestone_id)
 WHERE kind IN ('decision', 'question') AND milestone_id IS NOT NULL;

-- When the agent last said hello, for "project-wide decisions since your last session".
ALTER TABLE projects ADD COLUMN agent_hello_at INTEGER;
