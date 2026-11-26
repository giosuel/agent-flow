---
title: Milestones and steps
description: Milestones end with a tag; steps inside them start and conclude as the work does.
---

## Milestones

A milestone is a stretch of work that ends with a tag. They're set at the start of a project, in
order, and named by kind:

| Name | Kind |
| --- | --- |
| `M1`, `M2`, … | A main milestone. |
| `A3` | An addendum to `M3` after it closed: fixes, follow-ups, feedback. |
| `P4` | Preparation before `M4` starts. |

A milestone is planned, active or closed. Pushing its release's tag closes it.

## Steps

Inside a milestone the agent works in steps: a handful of tasks it plans in one `add_tasks` call,
each with an estimate.

- A step **starts by itself** when its first task goes to doing.
- The agent **concludes** it with `conclude_step` once its tasks are done, with a few lines on what
  it brought. AgentFlow adds the numbers, the PRs and the owner's decisions.
- Tasks still open must be moved, parked or dropped first, or all moved on with `move_open_to`.
- **Open PRs and untested builds never hold a step.** The agent stacks them and moves on.

A **review** is a step of its own: who reviewed (fresh agents with no context of the work), the
commit, the owner's words, and the reports as notes. Its findings go in as tasks, ranked by
importance; the ones it won't do go in as skipped, with the reason.

## Nothing is deleted

Milestones, steps, tasks, decisions and questions are the agent's to add, edit and archive.
Archiving hides them from lists and counts and keeps them findable (`include_archived`);
`restore_milestone` brings one back with what was archived along with it. Archiving a step can
move its tasks, notes, PRs and timings to another step instead, to fold small steps into one.

The **Plan** tab shows the milestones with their steps, each step's tasks in order, estimates
against time spent, and the summaries of concluded steps.
