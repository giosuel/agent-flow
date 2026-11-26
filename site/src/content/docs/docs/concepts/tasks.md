---
title: Tasks
description: States, estimates, time counted from states, and timings of builds and tests.
---

A task belongs to a step (or a milestone) and has an estimate. The agent sets its order with
`order_tasks`; the first open one shows as next.

## States

| State | Meaning |
| --- | --- |
| `todo` | Not started. |
| `doing` | Under way. Setting it also starts the task's step. |
| `review` | In review. The clock stops. |
| `done` | Finished. |
| `blocked` | Waiting. `blocked_by` names a question (anything that needs the owner) or a short note (an agent's review, a remote build). Answering the question unblocks it by itself. |
| `parked` | Not now; `resurface_when` says when it comes back, e.g. "after M5". |
| `skipped` | Won't be done, with a note on why. |
| `archived` | Hidden and kept. Any other state brings it back. |

`update_task` changes one task or several at once (`ids`), for example the three tasks one PR
carries out.

## Time

A task's time is the time it spent in doing. Blocked, review or todo stop the clock, and so does
the agent being away (no call for 30 minutes), so a task left in doing overnight doesn't count.
When several tasks of a step are in doing at once, the time is counted once and split evenly among
them. A concluded step shows its estimates against the time it took.

## Timings

Builds and tests get their own numbers: wall time, CPU seconds and peak memory. The easy way is to
run the command through `flow time`, which measures it, prints the numbers and logs them on a
task, a step or a PR:

```sh
flow time --on T-12 -- cargo test --workspace
```

Left out, `--on` means the one task in doing, or the one step under way. `log_timing` logs numbers
measured some other way. Jobs on a [build server](/docs/build-servers/) log theirs by themselves.

## What the agent is doing

The project page shows the tasks in doing by themselves. When that's not enough, `working_on`
sets one line, such as "T-14 and T-16, with 2 subagents", until the next `hello`.
