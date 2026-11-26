---
title: Test builds
description: Agents publish builds, the owner runs them with flow run and marks what to try.
---

## Publishing

When something is ready to try, the agent builds it and publishes it:

```sh
flow publish target/release/lumen --product lumen \
  --summary "Album sidebar" \
  --feature "Albums: open the sidebar and drag a photo onto an album" \
  --needs "glibc 2.39 or newer"
```

The path is a binary, or a folder with the app and its files (`--entry` names the executable that
starts it). AgentFlow keeps the archive as it came. Each product's builds are numbered, numbers are
never reused, and a build never changes once published. Its name is
`<date>-<time>-<topic>-<commit>`, from the checkout it was built from.

`--platform` says what it runs on (`linux-x86_64` by default). Only Linux x86_64 builds are offered
to `flow run`; others are a download on the build's page. A build with at least one `--feature`
notifies the owner and waits for feedback.

## Running

On any of the owner's connected machines:

```sh
flow run                 # the products
flow run lumen           # the newest build
flow run lumen 7         # build 7 (or part of its name)
flow run lumen list      # every build of lumen
flow run lumen -- --debug  # arguments for the app
```

`flow run` downloads a build once, checks every file against its checksum, and starts it.

## To try

What the owner should try is a list per project, kept by the agent with `add_to_test`: a title,
how to try it, a category of the agent's choosing, and the build it comes with. `flow publish
--feature` adds its own.

The **Builds** tab leads with the newest build: its platforms, what it needs, the command that
starts it and a download. Under it, what's new in that build, each with three buttons:

| Button | What happens |
| --- | --- |
| **Works** | Tried and fine. It leaves the list. |
| **Problem** | You write what went wrong; the agent gets your words. It stays open. |
| **Skip** | Not tried and not needed. It leaves the list. |

Then everything else still to try, by category, and the earlier builds in a table. Open items come
along with every new build and release until they're tried. When the owner tells the agent in chat
instead, it marks them with `resolve_tests`.
