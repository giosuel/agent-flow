---
title: Releases
description: A planned tag that waits for its PRs, and the Push tag button that closes the milestone.
---

Every milestone ends with a tag. The agent plans it with `add_release`:

| Field | Example |
| --- | --- |
| `tag` | `v0.4.0-m3`, or `v1.3.0` for a release |
| `title` | `M3: Albums and tags` |
| `waits_for` | `["PR-51", "PR-52"]`: PRs that must be merged before tagging |
| `body` | Release notes, or what the tag closes, in Markdown |
| `milestone` | `M3`, the milestone it closes |

## Push tag

The release gets its **Push tag** button by itself: an annotated tag on the newest commit of
`main`, made through GitHub's API with `gh`, so the server needs no checkout. It stays locked until
every PR it waits for is merged ("unlocks once #52 is merged"). Pressing it pushes the tag and
closes the milestone.

To tag another branch, the agent passes `tag_button: false` and adds its own button with
`add_button`.

`update_release` changes the title and notes; the tag and the PRs it waits for stay as planned.

## What comes along

Everything the owner still has to try (see [Test builds](/docs/concepts/builds/)) is linked to each
new release, until it's tried.
