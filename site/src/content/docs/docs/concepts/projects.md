---
title: Projects
description: One project, one agent, one document of rules.
---

A project is one piece of work with one agent. Its short name, the slug, is part of every record
id and URL: `lumen/T-12` is task 12 of the project `lumen`.

## The agent and its token

Each project's agent connects with the project's token. That token can change only its own
project and read all the others. The agent is the only one who writes the project's records;
subagents report back to it, apart from handing in files with `flow note`.

Other tokens: a machine's token (from `flow connect`) only reads; the consultant's can change any
project; a builder's is only for building. Merge and Push tag buttons can only be pressed from the
owner's signed-in browser.

## The project document

One Markdown document per project, the agent's to shape: how to work on the project (commit rules,
tests, tools), its stack and platform, the roadmap if it has one, and pointers. Think of it as a
per-project `AGENTS.md` that lives outside the repository.

`hello` returns it in full at the start of every session, so it stays short (16 KB at most) and
current. The agent changes one `##` section at a time with `edit_project_doc`; the owner opens it
from the project's overview and edits it as one document with a live preview. Every edit is a
revision.

## Details

`update_project` keeps the rest: description, GitHub repository, checkout, machine, default model
and effort, ports, the product name for test builds, and the merge method (merge, squash or
rebase) its Merge buttons use. A project is active, paused or archived.

## The pages

A project's page has the tabs **Overview** (what's waiting on you, the project, its milestones,
the next release, the latest decisions), **Plan**, **Questions**, **Pull requests**, **Builds**,
**Screenshots**, **Notes** and **Settings**. The **Dashboard** gathers what waits on you across
every project; on a phone it's the home-screen app's start page.
