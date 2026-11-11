-- The machine as a whole, once a minute, for the Machine page's cards: their trend and their last
-- hour (the owner, 2026-11-11, picked cards with a sparkline: "lets go with C").
CREATE TABLE machine_totals (
    at INTEGER PRIMARY KEY,
    cpu REAL NOT NULL,           -- busy cores
    mem_gb REAL NOT NULL,        -- used
    disk_free_gb REAL NOT NULL,  -- /
    tmp_free_gb REAL NOT NULL,   -- /tmp
    load1 REAL NOT NULL
);
