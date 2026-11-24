-- A running job's request on its builder, so a restarted server follows it again instead of
-- failing it (the owner, 2026-11-17: installs restarted the server under Imaginarium's jobs).
ALTER TABLE remote_jobs ADD COLUMN req_id INTEGER;
