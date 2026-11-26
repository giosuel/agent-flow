---
title: The log
description: One row for everything that happens, kept for good.
---

Every request is authenticated, and everything that happens is written to the activity log as it
happens: each MCP call, each API call from `flow`, each page the owner opens, each button press,
each build job, the conversations between sessions. Nothing in it is ever deleted.

## The Log page

The **Log** has one category per source as its main filter:

| Category | What's in it |
| --- | --- |
| Agents | MCP calls from the agents. |
| API | `flow` and the JSON API. |
| Builds & jobs | Jobs on build servers; each is one row whose output folds open. |
| Conversations | What sessions and their subagents told each other. |
| You | The owner's pages and presses. |
| System | The GitHub poller and the server's own jobs. |

Project, actor and severity are dropdowns; the time range is 1 h, 24 h, 7 d, 30 d or all. A
histogram shows the entries over that range, and clicking a bar zooms in. The search takes words
and fields:

```text
severity:warn project:lumen -action:read merge failed
```

The fields are `severity`, `project`, `actor`, `kind`, `action`, `via`, `target`, `ip` and `cat`; a
`-` in front negates one. The page streams live by default (**Pause** stops it), loads older
entries as you scroll, and folds identical entries in the same second into one row with ×N.

## Keeping it

Reads older than 90 days move to one zstd-compressed file per month in `<data_dir>/log-archive/`;
a search reaches them when its time range does. Changes, messages and everything else stay in the
database.

## Around it

- **Conversations**: what sessions tell each other with Claude Code's `SendMessage`, and what each
  session's subagents were told and reported, read from the transcripts that `flow sync` sends.
- **Usage**: token counts per day (per hour when a day is opened), one line per agent, a table per
  project, the costliest tasks, and the plan's 5-hour and weekly windows. Agents read the same with
  the `usage` tool or `flow usage`.
- **Machine**: the server's load, memory and disks, CPU and memory per project over time (each
  agent with everything it started), the ports projects claim, and the build servers.
- **Notifications**: the bell keeps every notification for 30 days, whether or not a device got it
  as a push.
