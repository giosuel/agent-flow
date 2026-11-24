-- SSH builders are gone (the owner, 2026-11-17). Each becomes a builder that waits for
-- `flow builder add` on its machine, which takes it over: its name, the projects that picked it and
-- its job history stay. Its SSH target and key are forgotten.
UPDATE build_servers SET kind = 'daemon', ssh = '', key_file = '' WHERE kind != 'daemon';
