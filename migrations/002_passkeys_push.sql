-- M1: passkeys, push subscriptions and the owner's settings.

-- The owner's passkeys. `passkey` is webauthn-rs's serialised credential (public key, counter).
CREATE TABLE passkeys (
    id           INTEGER PRIMARY KEY,
    cred_id      BLOB NOT NULL UNIQUE,
    name         TEXT NOT NULL,
    passkey      TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER
);

-- Browsers that receive push notifications (one row per device the owner turned them on for).
CREATE TABLE push_subscriptions (
    id           INTEGER PRIMARY KEY,
    endpoint     TEXT NOT NULL UNIQUE,
    p256dh       TEXT NOT NULL,
    auth         TEXT NOT NULL,
    device       TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    last_ok_at   INTEGER,
    last_error   TEXT
);

-- Small owner settings: notification switches, the owner's WebAuthn user id.
CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
