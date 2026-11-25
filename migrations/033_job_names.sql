-- A build job's name: what it is, in a few words ("Tests for PR #170"), its title on its page
-- (the owner, 2026-11-25, D-275). Jobs from before keep '' and show their command instead.
ALTER TABLE remote_jobs ADD COLUMN name TEXT NOT NULL DEFAULT '';
