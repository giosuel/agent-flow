-- People (the owner, 2026-11-25: D-270, D-272, D-273, D-274): everyone who signs in in a browser.
-- The first one is the admin, made with `sudo flow server setup`; only the admin invites others.
-- Passkeys, browser sessions, push subscriptions and sign-in links belong to a person; everything
-- else stays shared.
CREATE TABLE people (
    id          INTEGER PRIMARY KEY,
    -- Picked at the first sign-in; NULL until then.
    username    TEXT UNIQUE COLLATE NOCASE,
    email       TEXT NOT NULL DEFAULT '',
    -- One of the 15 profile icons (src/web/avatars.rs), by name; '' until picked.
    icon        TEXT NOT NULL DEFAULT '',
    admin       INTEGER NOT NULL DEFAULT 0,
    -- The WebAuthn user handle of this person's passkeys.
    webauthn_id TEXT NOT NULL,
    invited_by  INTEGER REFERENCES people(id),
    created_at  INTEGER NOT NULL,
    removed_at  INTEGER
);
CREATE UNIQUE INDEX people_one_admin ON people (admin) WHERE admin = 1 AND removed_at IS NULL;

ALTER TABLE passkeys ADD COLUMN person_id INTEGER REFERENCES people(id);
ALTER TABLE web_sessions ADD COLUMN person_id INTEGER REFERENCES people(id);
ALTER TABLE push_subscriptions ADD COLUMN person_id INTEGER REFERENCES people(id);
-- Who a link signs in (NULL: someone new, for setup and invites) and who made it.
ALTER TABLE login_links ADD COLUMN person_id INTEGER REFERENCES people(id);
ALTER TABLE login_links ADD COLUMN made_by INTEGER REFERENCES people(id);

-- Today's owner becomes the first person, the admin, keeping their passkeys' user handle; their
-- username, email and icon come at their next sign-in.
INSERT INTO people (id, username, admin, webauthn_id, created_at)
SELECT 1, NULL, 1,
       COALESCE((SELECT value FROM settings WHERE key = 'owner.webauthn_id'), ''),
       CAST(strftime('%s', 'now') AS INTEGER) * 1000
WHERE EXISTS (SELECT 1 FROM passkeys)
   OR EXISTS (SELECT 1 FROM web_sessions WHERE revoked_at IS NULL)
   OR EXISTS (SELECT 1 FROM push_subscriptions);
UPDATE passkeys SET person_id = 1 WHERE EXISTS (SELECT 1 FROM people WHERE id = 1);
UPDATE web_sessions SET person_id = 1 WHERE EXISTS (SELECT 1 FROM people WHERE id = 1);
UPDATE push_subscriptions SET person_id = 1 WHERE EXISTS (SELECT 1 FROM people WHERE id = 1);
-- Unused links from before were the owner's own.
UPDATE login_links SET person_id = 1 WHERE used_at IS NULL AND EXISTS (SELECT 1 FROM people WHERE id = 1);

-- The bell's read marks are each person's own (D-270): opening the bell reads them for you only.
CREATE TABLE notification_reads (
    notification_id INTEGER NOT NULL REFERENCES notifications(id) ON DELETE CASCADE,
    person_id       INTEGER NOT NULL REFERENCES people(id),
    read_at         INTEGER NOT NULL,
    PRIMARY KEY (notification_id, person_id)
);
INSERT INTO notification_reads (notification_id, person_id, read_at)
SELECT id, 1, read_at FROM notifications WHERE read_at IS NOT NULL AND EXISTS (SELECT 1 FROM people WHERE id = 1);
-- What pushes go out is each person's choice too: today's choices become the admin's.
INSERT OR IGNORE INTO settings (key, value)
SELECT 'person.1.' || key, value FROM settings WHERE key LIKE 'notify.%' AND EXISTS (SELECT 1 FROM people WHERE id = 1);
