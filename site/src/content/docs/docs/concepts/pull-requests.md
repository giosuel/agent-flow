---
title: Pull requests
description: PRs in merge order, a Merge button on each, and stacks merged from the bottom.
---

Agents open pull requests on GitHub as usual. AgentFlow picks them up from the project's
repository by itself: every three minutes it reads each active project's open PRs and the ones it
still thinks are open. Merges, closes and failing CI become news for the agent.

## Merge order

The **Pull requests** tab lists a project's open PRs in merge order, worked out from their bases.
A PR based on another PR is **stacked**: "based on #51, merges after it".

Whether a PR is ready is GitHub's draft flag alone: a draft isn't, anything else is. To hold one
back, the agent makes it a draft again (`gh pr ready --undo <n>`).

## Merge buttons

Every PR gets a **Merge** button by itself. It merges with the project's merge method (merge,
squash or rebase) and deletes the branch, so GitHub moves the next PR of a stack onto `main`.

A stacked PR merges only together with the PRs below it, from the bottom. It shows one button,
**Merge #51–#53**, instead of its own: the PRs merge one after the other and stop at the first
that fails. The PRs below need to be ready apart from CI; the one it ends at needs passing CI
too, so `main` ends up green.

The page always shows a button's exact command before the press, and the press carries a hash of
what was shown, so the command can't change between looking and pressing. The commands run as the
agents' user, with `gh`.

## Buttons for anything else

For something unusual, an agent can put its own button on a PR or a release with `add_button`: the
exact `git` or `gh` commands, at most 40 lines and 2,000 characters, because the owner reads all of
it. Buttons can't be edited; `withdraw_button` takes one back. Agents never merge or tag
themselves.

## Linking work

A PR whose branch doesn't say which tasks it carries out can say so with `update_pr`
(`implements: ["T-14"]`). The Dashboard lists PRs ready to merge under **Merge and release**; they
aren't counted in the badge.
