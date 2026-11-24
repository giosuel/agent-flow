---
title: Configuration
description: The server's config.toml, the client's files, and the environment variables.
---

## The server

`~/.config/agentflow/config.toml` for a single-user install, `/etc/agentflow/config.toml` for the
system service (`agentflow setup` writes it). Every field is optional; unknown fields are refused.

```toml
listen = "127.0.0.1:8790"              # never a wildcard address: refused at start
public_url = "https://flow.example"    # cookies, the passkey domain, allowed hosts
data_dir = "~/.local/state/agentflow"  # database, keys, builds, backups, log archive
trusted_proxies = ["127.0.0.1", "::1"] # may set X-Forwarded-For (your reverse proxy)
session_idle_days = 14                 # the owner's session ends after this long idle
session_max_days = 30                  # and after this long in any case
token_dir = "~/.config/agentflow/tokens"  # token files for Claude Code's headersHelper
push_contact = "mailto:you@example.com"   # the sender contact push services ask for
build_free_gb = 10                     # disk a build upload must leave free
transcripts_dir = "~/.claude/projects" # Claude Code transcripts, for token usage
local_transcripts = false              # also read every transcript on this machine
external_token_file = ""               # GitHub token for other people's repositories
git_host = "https://github.com"        # where drafts' repositories are fetched and pushed
flow_bin = ""                          # the flow handed out; empty: the one next to the server
admin_socket = ""                      # for `flow server add-passkey`; empty: the default

# When AgentFlow runs as its own user (agentflow setup writes these):
agents_home = "/home/agents"           # where ~/ points: the agents' home
hands_socket = "/run/agentflow-hands/hands.sock"  # the agents' helper
```

| Field | Notes |
| --- | --- |
| `listen` | `0.0.0.0` or `[::]` is refused: put a reverse proxy in front. |
| `public_url` | Must start with `http://` or `https://`. Passkeys and push need https on a domain name. |
| `local_transcripts` | Off: conversations come from each project's hooks (`flow sync`), wherever they run, and nothing else on the machine is read. |
| `external_token_file` | A token with `public_repo`, read only when the owner presses Send. The token can also be set in **Settings › GitHub**. |
| `handbook_dir` | The old ai-handbook checkout, imported once while the handbook is empty. |

Still accepted so older configs load, and no longer used: `handbook_repo`, `git_author`,
`github_writes`, `build_servers`.

### GitHub tokens

**Settings › GitHub** holds two tokens: one for your own repositories (Merge and Push tag, reading
PRs, importing PRs) and one for other people's (sending contributions). They're kept encrypted in
the database with a key in its own file in the data folder, so a copy of the database alone doesn't
reveal them. Without them, `gh` uses the login of the user it runs as.

## The data folder

| Path | What's in it |
| --- | --- |
| `flow.db` | The SQLite database (WAL mode). |
| `backups/flow-<date>.db` | The daily copy; the last 30 are kept. |
| `export/<slug>.md` | Every project as Markdown, readable without AgentFlow. |
| `log-archive/` | Reads older than 90 days, one zstd file per month. |
| `builds/` | Test builds, as they were published. |
| `screenshots/`, `notes/` | Uploaded screenshots and note files. |
| `handbook/` | The handbook's files as plain files. |
| `outgoing/` | AgentFlow's own repositories for contributions. |
| `blobs/` | Files `flow remote` sent, by their content. |
| `vapid.key` | The server's key for signing push messages. |
| `secret.key` | The key the GitHub tokens are encrypted with. |

## A machine with `flow`

| File | What it is |
| --- | --- |
| `~/.config/agentflow/flow.toml` | The server `flow connect` saved. |
| `~/.config/agentflow/token` | This machine's token. |
| `~/.config/agentflow/tokens/<project>.token` | A project's token, for its agent. |
| `~/.config/agentflow/builder.toml` | A builder's config, written by `flow builder add`. |
| `~/.local/share/agentflow/handbook/` | The handbook's copy. |

| Variable | What it does |
| --- | --- |
| `AGENTFLOW_URL` | The server, instead of the saved one. |
| `AGENTFLOW_TOKEN_FILE` | The token file; a project's folder sets it for its agent. |
| `AGENTFLOW_CONFIG` | The server's config file. |
| `AGENTFLOW_BUILDER_CONFIG` | A builder's config file. |
