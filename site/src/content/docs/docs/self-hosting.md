---
title: Self-hosting
description: Running AgentFlow as its own user or as one user, https, passkeys, access, backups.
---

AgentFlow runs on one machine, usually the one the agents work on. There are two ways to run it.

## As its own user

Recommended on a shared machine: the agents can't read AgentFlow's database, tokens or sessions.
Install the programs (a package, or the binaries where only root can change them), then:

```sh
sudo agentflow setup --public-url https://flow.example --agents-user <user> --dry-run  # the steps
sudo agentflow setup --public-url https://flow.example --agents-user <user>
# then, as the agents' user:
systemctl --user daemon-reload; systemctl --user enable --now agentflow-hands
```

Setup creates the user `agentflow`, writes `/etc/agentflow/config.toml` (an existing one is kept),
installs the system service (data in `/var/lib/agentflow`, no access to `/home`) and
`agentflow-hands`, a user service of the agents' user, and turns on linger so the helper runs
without a login. `--import <dir>` brings a single-user instance's data along; stop that one first.

The server runs as `agentflow`. What has to happen as the agents' user goes to `agentflow hands`
over a Unix socket only `agentflow` may use: the owner's buttons, `gh` with the agents' login, token
files, new projects' folders, reading transcripts, and finding which folder an agent process works
in.

**Packages.** There are two, as `.deb` and on the AUR:

- `agentflow`, the server: the `agentflow` program, its system service, the helper's user service
  and the `agentflow` user. It needs `agentflow-client`, since agents on the server use `flow`.
- `agentflow-client`: `flow`, and `agentflow-builder` with its user service, for every machine that
  connects or builds.

## As one user

On a personal machine, `scripts/install.sh` in a checkout builds the release programs, installs
them as `~/.local/bin/agentflow`, `flow` and `agentflow-builder`, and runs the server as a systemd
user service (`packaging/agentflow.service`). It backs up the database before it replaces anything.
Everything runs as you, so agents running as you can read its files.

`agentflow managed-settings` prints Claude Code managed settings that keep every session's Read and
Edit tools away from AgentFlow's tokens, data and config, and the root command to install them. Shell
commands run as the same user can still read those files; a user of its own is the stronger fence.

## https, passkeys and push

Passkeys and push notifications need https on a domain name. With Caddy in front:

```text
flow.example {
    reverse_proxy 127.0.0.1:8790
}
```

and in the config `listen = "127.0.0.1:8790"`, `public_url = "https://flow.example"`. On a
plain-http address, such as a VPN IP, everything else works and sign-in uses one-time links.

The web app installs to a phone's home screen (on iPhone: Share, then Add to Home Screen; iOS 16.4
or later). Open it from there, go to **Settings** and press **Turn on for this device**. Push
messages are encrypted for each device (RFC 8291) and signed with the server's VAPID key. Settings
has a switch for each kind: new question, build ready to try, something to review, and urgent
calls from an agent.

## Access

Every request is authenticated. Only the sign-in page, asking to connect a machine, the install
script and the `flow` download are public.

- **People** sign in with a passkey. The first account is the admin: `sudo flow server setup` on
  the server prints a one-time link (15 minutes) that makes it and its passkey. Only the admin
  invites others (Settings › People, or `sudo flow server invite`, a link for 7 days). A new
  device gets a link from Settings › Passkeys or `sudo flow server add-device [username]`; an
  admin who lost every passkey runs `setup` again. The commands go through a socket only root
  may use; every link is logged and pushed.
- **Machines** connect with `flow connect` (or the install script): the owner approves in the
  browser, and the machine gets a read-only token of its own.
- **The owner's admin jobs** from a connected machine (tokens, projects, backups, importing PRs)
  wait for approval in the browser, every time. A token on disk is never enough for them.
- **Tokens** stand for a project's agent (changes its own project, reads the rest), the consultant
  (changes anything), a machine (reads) or a builder (builds). Only the owner's browser presses
  Merge and Push tag. Settings lists the tokens and rotates them.

## Backups

Once a day the database is copied to `<data_dir>/backups/flow-<date>.db` (the last 30 are kept),
and every project is exported as Markdown to `<data_dir>/export/<slug>.md`. `flow backup` does both
at once, with the owner's approval; `agentflow backup` copies the database right away on the server.

## The handbook and the consultant

The **handbook** is the owner's rules for every project: how they work, the machine, procedures.
Its files live in the database, each edit a revision, and every machine keeps a copy in
`~/.local/share/agentflow/handbook`, which `~/.claude/CLAUDE.md` includes. Agents read it with the
`handbook` tools; the owner edits it on the Handbook page.

The **consultant** is an optional Claude Code session the server runs, switched on in Settings. It
maintains the handbook and answers agents' questions about how the owner works. It needs Claude
Code and `tmux` for the server's user, signed in once (`sudo -u agentflow claude auth login`).
