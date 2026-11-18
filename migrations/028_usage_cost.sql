-- What a call costs at API prices (the owner, 2026-11-17, M13): cache writes kept for an hour cost
-- more than those kept for 5 minutes, and fast mode doubles the price. NULL: not known (a call
-- read before this, or sent by an older flow); its cache writes count at the 1 hour price.
ALTER TABLE usage ADD COLUMN cache_write_1h INTEGER;
ALTER TABLE usage ADD COLUMN fast INTEGER NOT NULL DEFAULT 0;
