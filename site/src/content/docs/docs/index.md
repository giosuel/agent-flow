---
title: Introduction
description: What AgentFlow is, its three programs, and how work moves through it.
---

AgentFlow is project management for a team of Claude agents and the person they work for.

Each project has one agent. Instead of keeping Markdown plans in the repository, the agent records
its milestones, steps, tasks, questions, decisions, pull requests, releases, test builds, notes and
screenshots in AgentFlow over MCP. The owner works from a web app: they answer questions, press
**Merge** and **Push tag**, try builds and send contributions to other people's repositories.
Every call, page view and button press is logged.

## Three programs

| Program | What it does |
| --- | --- |
| `agentflow` | The server. A web app for the owner, the MCP endpoint for agents (`/mcp`, streamable HTTP), a JSON API for `flow`, and a SQLite database. Runs under systemd. |
| `flow` | The command people and agents type: connect a machine, set up a project, publish and run test builds, upload screenshots and notes, run builds on a build server, draft contributions, wait for news. |
| `agentflow-builder` | A build machine's background program. It connects out to the server and runs the builds and tests agents send with `flow remote`. |

## How work moves

1. An agent starts a session. A Claude Code hook runs `flow hello`, which hands it the project
   document, where it is (the steps under way, tasks in doing, what's blocked) and the news.
2. It plans a step with `add_tasks`, each task with an estimate, and moves tasks to doing and
   done. Time is counted from those states.
3. When it needs a decision it calls `ask_question`, with numbered options and its recommendation.
   The owner gets a push notification and answers in the web app. The answer becomes a
   decision and unblocks the tasks that waited for it.
4. It opens pull requests on GitHub. AgentFlow picks them up, puts them in merge order, and gives
   each a Merge button. A stack merges from the bottom with one **Merge #a–#b** button.
5. It plans the milestone's tag with `add_release`. Once the PRs are merged the owner presses
   **Push tag**, which closes the milestone.
6. It publishes a test build with `flow publish`. The owner starts it on any machine with
   `flow run` and marks each thing to try **Works**, **Problem** or **Skip**.

Agents never merge, tag, or post to other people's repositories themselves. They prepare the
command or the draft; the owner's press is the approval.

## What it runs on

The server and `flow` are Rust programs for Linux. The web app is server-rendered HTML with a
little JavaScript and no CDN: it works with nothing but AgentFlow reachable. It installs as a
home-screen app on a phone and sends Web Push notifications. Agents are
[Claude Code](https://claude.com/claude-code) sessions; merges and tags go through `gh`.

Next: [Getting started](/docs/getting-started/).
