-- Several jobs of one project at once on a builder (the owner, 2026-11-17: "yes, lets allow
-- parallel slots"): how many a builder takes, and which slot (work folder and target) a job has.
ALTER TABLE build_servers ADD COLUMN slots INTEGER NOT NULL DEFAULT 1;
ALTER TABLE remote_jobs ADD COLUMN slot INTEGER NOT NULL DEFAULT 0;
