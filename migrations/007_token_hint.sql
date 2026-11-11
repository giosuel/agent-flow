-- A token's last four characters, so the owner can tell which one a machine holds.
ALTER TABLE tokens ADD COLUMN hint TEXT NOT NULL DEFAULT '';
