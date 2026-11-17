-- Every notification AgentFlow sends, kept for the bell at the top (the owner, 2026-11-17: "a
-- notification overlay that stores incoming notifications"), whether or not a device got a push.
-- Kept 30 days; opening the bell marks them all read.
CREATE TABLE notifications (
    id       INTEGER PRIMARY KEY,
    at       INTEGER NOT NULL,
    kind     TEXT NOT NULL,          -- question, build, review, urgent, test
    title    TEXT NOT NULL,
    body     TEXT NOT NULL DEFAULT '',
    url      TEXT NOT NULL DEFAULT '',
    tag      TEXT,                   -- a newer unread one with the same tag replaces it
    read_at  INTEGER
);
CREATE INDEX notifications_at ON notifications (at);
CREATE INDEX notifications_unread ON notifications (read_at) WHERE read_at IS NULL;
