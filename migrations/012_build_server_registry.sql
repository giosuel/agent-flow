-- Build servers are added and removed on the Settings page (the owner, 2026-11-11: "register a
-- host via SSH and install the cache stuff (optionally) throgh the agent flow interface. lets add
-- this to settings, a new card for build servers and each project can then pick a build server in
-- their settings with a dropdown"). The ones in config.toml are copied in at start.
CREATE TABLE build_servers (
    name TEXT PRIMARY KEY,
    ssh TEXT NOT NULL,
    -- A key AgentFlow made for it (in its data folder), or '' for ssh's own config.
    key_file TEXT NOT NULL DEFAULT '',
    root TEXT NOT NULL DEFAULT 'agentflow',
    min_threads REAL NOT NULL DEFAULT 8,
    min_memory_gb REAL NOT NULL DEFAULT 16,
    reserve_memory_gb REAL NOT NULL DEFAULT 8,
    keep_days INTEGER NOT NULL DEFAULT 14,
    created_at INTEGER NOT NULL
);

-- The build server each project's `agentflow remote` uses; none: it builds where the agent is.
CREATE TABLE project_build_server (
    project_id INTEGER PRIMARY KEY REFERENCES projects (id),
    server TEXT NOT NULL
);
