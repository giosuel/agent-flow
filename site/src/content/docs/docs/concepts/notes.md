---
title: Notes and screenshots
description: Files and short notes handed in by agents, and screenshots for the owner.
---

## Notes

A note is anything worth keeping that isn't a task or a decision: a known issue, a technical note,
an estimate, a morning report, a plan, a benchmark, a log. Each is a record (`N-4`) with a name and
a category of the agent's choosing.

Files go up with `flow note`, any kind up to 25 MB:

```sh
flow note report.md --name "Morning report, 12 Nov" --category "morning report"
```

Short text notes come from the `add_note` tool. Markdown and plain text render on the page; other
files are downloads. `--replaces N-6` uploads a new version and keeps the old one.

A note can come back to the agent in `hello`: on a date (`--resurface 2026-11-25`) or until it's
resolved ("when Loro updates"). `update_note` resolves a note that no longer holds, with the reason.

Handovers and reports meant for another session or for later go here, not into a scratchpad, and
are referred to by their id, which the owner can click. Subagents may run `flow note` too; their
notes are marked as a subagent's.

## Screenshots

Agents show the owner what they see with `flow screenshot`: PNG, JPEG or WebP up to 10 MB.

```sh
flow screenshot sidebar.png --name "Album sidebar, variant A" --category mockup --record T-31
```

The category is free text (test build, mockup, spike test, bug, …). Each screenshot is a record
(`S-4`) on the **Screenshots** tab, which filters by category, and opens full size on its own page.
With `--record` it's linked to that task, build or question and shows there too.

A mockup carries its design link (`--design-url`) and a state: proposed, approved, or built once the
PR that implements it merges (`update_screenshot`).
