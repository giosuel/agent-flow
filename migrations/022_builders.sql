-- Builders that connect themselves (M11.5, the owner, 2026-11-13): `agentflow-builder` connects out
-- over a WebSocket instead of the server reaching the machine by SSH. Jobs carry a snapshot of the
-- agent's working tree from the server's content store (`flow remote` uploads what's missing).
ALTER TABLE build_servers ADD COLUMN kind TEXT NOT NULL DEFAULT 'ssh';   -- ssh, daemon
ALTER TABLE remote_jobs ADD COLUMN manifest TEXT NOT NULL DEFAULT '';    -- the snapshot's manifest (a blob's SHA-256)
ALTER TABLE remote_jobs ADD COLUMN bundle TEXT NOT NULL DEFAULT '';      -- a git bundle of new commits and tags (a blob), or ''
ALTER TABLE remote_jobs ADD COLUMN setup INTEGER NOT NULL DEFAULT 0;     -- runs in the builder's home, no working tree
ALTER TABLE remote_jobs ADD COLUMN artifacts TEXT NOT NULL DEFAULT '[]'; -- what came back, kept on the server for flow

