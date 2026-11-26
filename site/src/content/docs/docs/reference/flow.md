---
title: The flow command
description: Every flow command, for the owner's machines and for agents.
---

`flow` talks to an AgentFlow server over its API. `flow --help` and `flow <command> --help` print
the same with every option.

**Which server and token.** `--url` (or `AGENTFLOW_URL`), else the server `flow connect` saved in
`~/.config/agentflow/flow.toml`. `--token-file` (or `AGENTFLOW_TOKEN_FILE`), else
`~/.config/agentflow/token`. A project's folder sets both for its agent.

## Connecting a machine

| Command | What it does |
| --- | --- |
| `flow connect <url>` | Connect this machine. You approve it in the browser, which shows the same short code as the terminal; the machine gets a read-only token of its own. |
| `flow disconnect` | Revoke this machine's token and remove it here. |
| `flow status` | The server, whether it's reachable, both versions and who you are. |
| `flow update` | Replace this `flow` with the one the server hands out. |
| `flow version` | Print the version (also `flow -v`). |

## Test builds

| Command | What it does |
| --- | --- |
| `flow run [product] [build] [-- args]` | Fetch a build, check every file against its checksum, and start it. No product lists the products; `list` lists a product's builds; no build means the newest. |
| `flow publish <path> --product P --summary S` | Publish a test build (agents). `--feature "Title: what to try"` (repeat), `--entry`, `--platform`, `--needs`, `--tag`, `--checkout` or `--commit`/`--branch`. |

## Projects

| Command | What it does |
| --- | --- |
| `flow project setup [folder]` | Set a project up in this folder: with `--code` from its page, or press Enter to create a new one (`--name`). Safe to run again. |
| `flow project add <slug> --name N` | Add a project, with your approval in the browser. |
| `flow project list` | Every project with its folder. |

## The agents' commands

| Command | What it does |
| --- | --- |
| `flow hello` | The `hello` tool's reply as text, for a SessionStart hook (`--doc` and `--no-doc` split it to fit a hook's 10,000 characters). |
| `flow news [--again 1h]` | What's new since the agent last read its news; nothing when there's none. For a hook before each prompt. |
| `flow wait [--record Q-12] [--minutes 480]` | Block until something happens in the project, print it and exit (2 on timeout). Run it in the background. |
| `flow screenshot <file> --name N --category C` | Upload a screenshot: PNG, JPEG or WebP up to 10 MB. `--description`, `--record T-12`, `--design-url`, `--state`. |
| `flow note <file> --name N --category C` | Hand in a file as a note, up to 25 MB. `--record`, `--resurface`, `--replaces N-6`. |
| `flow time [--on T-12] -- <command>` | Run a command and log its wall time, CPU and peak memory on a task, step or PR. Exits with the command's code. |
| `flow remote [--fetch path] -- <command>` | Run a build or test on a build server and stream its output. See [Build servers](/docs/build-servers/). |
| `flow contribute pr\|issue\|comment\|commits` | Draft a contribution for someone else's repository. See [Contributions](/docs/concepts/contributions/). |
| `flow usage [--all] [--json]` | The plan's 5-hour and weekly windows, and what this project's tokens cost at API prices. |
| `flow sync [--all]` | Send what's new in the folder's Claude Code conversations. Hooks run it after each reply. |
| `flow statusline` | Claude Code's status line: the plan's windows, sent to AgentFlow at most once a minute. |
| `flow handbook sync` | Bring this machine's copy of the handbook up to date. Session starts do it too. |
| `flow mcp-headers --token-file F` | Print the `Authorization` header for Claude Code's `headersHelper`. |

## The owner's admin jobs

These wait for your approval in the browser, every time. Agents' tokens can't ask for them.

| Command | What it does |
| --- | --- |
| `flow token create project <project> [--file F]` | A token for a project's agent (`consultant` for the consultant). |
| `flow token list` / `revoke <id>` / `rotate <id>` | Every active token without its secret; revoke one; replace one. |
| `flow backup` | Back up the database and export every project now. |
| `flow import-prs <project> 300-374` | Bring a project's older PRs in as records. |
| `flow builder add` / `list` / `edit` / `remove` / `status` | Build machines. See [Build servers](/docs/build-servers/). |
| `sudo flow server add-passkey` | On the server: a one-time link (15 minutes) to add a passkey. |

## The server's commands

`agentflow` is the server itself, started by systemd. Its commands:

| Command | What it does |
| --- | --- |
| `agentflow serve` | Run the server. |
| `agentflow setup --public-url U [--agents-user U] [--listen A] [--import DIR] [--dry-run]` | Make it a system service running as its own user. |
| `agentflow backup [--name pre-install]` | Back up the database now; opens it read-only, so the server may keep running. |
| `agentflow managed-settings` | Print the root command that installs Claude Code managed settings keeping agents' file tools away from AgentFlow's files. |
| `agentflow hands` | The helper that works as the agents' user (its service runs it). |
