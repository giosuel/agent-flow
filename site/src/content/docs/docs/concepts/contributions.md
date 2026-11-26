---
title: Contributions
description: PRs, issues and comments for other people's repositories, drafted by agents and sent by the owner.
---

Agents never post or push to other people's repositories themselves, even when a token would let
them. They draft; the owner reads the draft in AgentFlow and sends it, as themselves.

## Drafting

A new PR, from the agent's checkout: the commits on top of their branch travel as a git bundle.

```sh
flow contribute pr --repo someone/formulary --base main --branch center-fences \
  --title "Center fence delimiters on the math axis" --body-file pr.md \
  --why "Fixes the misaligned fences in our exports" --agreed D-31
```

`flow contribute issue`, `comment --on C-4` and `commits --on C-4` (new commits for a PR already
sent) do the rest; the MCP tool `draft_contribution` drafts issues and comments too. A revised
draft after the owner asked for changes names the old one with `--revise C-7`.

## Reviewing and sending

AgentFlow reads the commits into a repository of its own and shows the owner the diff it computed,
the text and a few checks: whether the text has every heading of their PR template, and whether a
decision of the owner's agreed to it. The owner can edit the text, then:

| Button | What happens |
| --- | --- |
| **Send** | AgentFlow pushes to the owner's fork and opens the PR, issue or comment as them, with exactly that text. |
| **Ask for changes** | The note goes to the agent word for word. |
| **Drop** | The draft is closed. |

Sending needs a GitHub token for other people's repositories, set in **Settings › GitHub** (or as
`external_token_file` in the config). It's read only when the owner presses Send.

## Watching

Everything sent is watched every 30 minutes: new comments, reviews, a merge or a close go to the
project's agent, which answers with a new draft. A PR or issue sent before AgentFlow can be watched
too, with `track_contribution`.
