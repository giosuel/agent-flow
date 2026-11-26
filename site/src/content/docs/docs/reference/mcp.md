---
title: MCP tools
description: The tools AgentFlow's MCP server gives agents, from the tool descriptions in src/mcp.
tableOfContents:
  minHeadingLevel: 2
  maxHeadingLevel: 2
---

AgentFlow serves MCP over streamable HTTP at `/mcp`. Every request carries a token: a project's
agent changes only its own project and reads all others. Every call is logged, and the reply of a
write call carries the owner's news (answers, button presses, feedback) as it comes.

This page follows the descriptions in `src/mcp/`. The server's instructions, which every agent sees
when it connects, sum up how to use them.

## Session

### `hello`
Start every session with it. Returns the project, its document, milestones, where you are (steps
under way, tasks in doing, the next one, what's blocked), open questions, notes coming back and what
changed since the last session. `brief: true` after the context was reset or summarized: where you
are, news and notes, without the document. Takes `session`, `machine`, `model`, `effort`.

### `since_last_check`
What happened in the project since you last checked that you didn't do yourself: the owner's
answers and decisions, button presses, messages, feedback. Marks them read. `again: "1h"` shows the
last hour again.

### `whoami`
Who AgentFlow thinks you are: project, the token's kind, the server version.

### `list_projects`
All projects: name, what it is, status, whether its agent is online.

### `project_info`
A project's details, its document and milestones with task counts. Your own project if `project` is
left out.

### `update_project`
Change the project's details: description, repo, checkout, machine, default model and effort,
ports, build product, `merge_method` (merge, squash or rebase). Everything else goes in the
document.

### `edit_project_doc`
Change the project document: one `##` section at a time (replaced, added, or removed with empty
text), or the whole document. 16 KB at most.

### `working_on`
One line on what you're doing right now, for the project page, when it says more than the tasks in
doing. Cleared at the next `hello`.

### `usage`
The plan's 5-hour and weekly windows (% used, when they reset, what 100% is in dollars at API
prices), and the project's tokens and their cost today, in the current window and over 7 days, with
its costliest tasks. `all: true` for every project.

## Plan

### `add_milestone`
Add a milestone (`Mn`, `An` for an addendum, `Pn` for preparation) or, with `step_of`, a step
inside one. `review` makes it a review step. `before` inserts it before another.

### `update_milestone`
Rename, retitle, describe or move a milestone or step (`move_before`, `step_of`); set its state
(planned, active, closed), dates, summary and closing tag.

### `get_milestone`
A milestone or step in full: description, state, dates, summary, tag, review details, steps, its
tasks in order, and its notes and decisions.

### `conclude_step`
Conclude a step once its tasks are done, with a few lines on what it brought. Open tasks must be
moved, parked or dropped first, or all moved with `move_open_to`.

### `archive_milestone`
Archive a step, or a milestone with its steps, tasks, and the decisions and questions scoped to it.
`move_to` moves a step's tasks, notes, PRs and timings to another step instead. Needs a `reason`.

### `restore_milestone`
Bring back an archived step or milestone with what was archived along with it.

### `add_task`
Add one task to a step, with an estimate.

### `add_tasks`
Add a step's tasks in one call, in the order you'll do them, each with an estimate. Done tasks
brought in as history take `started_at` and `done_at`. All are added or none.

### `update_task`
Change a task, or several with `ids`: state (`todo`, `doing`, `review`, `done`, `blocked`,
`parked`, `skipped`, `archived`), title, details, step, tags, estimate, branch, `blocked_by`,
`resurface_when`, `time_spent` (a correction, with a note).

### `order_tasks`
Put a step's tasks in the order you'll do them; the first open one shows as next.

### `log_timing`
Log how long a build or test took (`wall_s`, `cpu_s`, `peak_mb`, `place`) on a task, step or PR.
`flow time` measures and logs it for you.

## Questions and decisions

### `ask_question`
Ask the owner one question, with context, numbered `options` and the `recommended` one. `scope` is
where the answer's decision lands; `blocks` names the tasks that wait; `going_ahead` builds the
recommendation meanwhile; `related` names decisions it touches; `in_chat` skips the notification;
`urgent` sends an urgent one.

### `update_question`
Change questions, one or several: scope, title, details; archive one that's no longer needed (it
stops blocking its tasks) or bring it back. With a `reason`.

### `withdraw_question`
Withdraw an open question that no longer needs an answer.

### `answer_question`
The owner answered in chat: record their words (and the `option` they picked). Unblocks the tasks
and records the decision in the question's scope. Answers given in AgentFlow need nothing.

### `record_decision`
Record a decision the owner made: one statement, why, `owner_words` verbatim, `said_at`, `source`
("owner" or "relayed by …"), and the question it `answers` or decision it `supersedes`.

### `record_decisions`
Several decisions in one call, oldest first. `"#3"` refers to the third of the list. All or none.

### `update_decision`
Change decisions, one or several: title, why, tags, a wrongly recorded quote or date; move to
another `scope`; mark `replaced_by` a newer one; archive. The old text stays in the history.

### `add_adr` and `add_adrs`
Point to ADR files in a repository (`repo`, `path`, `number`) and the decision each records. The
ADRs stay in the repository.

## Pull requests and releases

### `pull_requests`
A project's open PRs in merge order, each with whether it can be merged and why not, and its
buttons; plus releases waiting to be tagged.

### `update_pr`
Say which tasks a PR carries out (`implements`), when its branch doesn't. Also adds a PR AgentFlow
hasn't seen yet. Readiness is GitHub's draft flag.

### `add_release`
Plan a tag that waits for PRs: `tag`, `title`, `waits_for`, `body`, the `milestone` it closes. Its
**Push tag** button comes by itself (an annotated tag on `origin/main`); `tag_button: false` to add
your own.

### `update_release`
Change a planned release's title or notes. The tag and its PRs stay as planned.

### `add_button`
Only for something unusual: put a button on a PR or release with the exact commands the owner runs
by pressing it. `kind` is `merge`, `tag` or `other`; `requires` names records that must be merged
first. At most 40 lines and 2,000 characters.

### `withdraw_button`
Take back a button that hasn't run, or failed.

## Builds and testing

### `builds`
The project's test builds, newest first: number, name, what's in it, whether the owner tried it,
and their feedback. Builds are published with `flow publish`.

### `update_build`
Change a build: what the owner should try (`features`), its `tag`, or `withdraw` it.

### `add_to_test` and `add_to_tests`
Add things for the owner to try: `title`, `how`, a `category` of your choosing, and the `build` it
comes with. They stay on the Builds tab until the owner marks them Works, Problem or Skip.

### `resolve_tests`
After the owner told you how testing went: mark items `works`, `problem` (stays open, with their
words) or `dropped`.

## Notes and screenshots

### `add_note`
A short note: a known issue, a technical note, an estimate. A `category`, the `text`, the `record`
it belongs to, and when it should `resurface` in `hello`. Files go up with `flow note`.

### `update_note`
Change a note's name, category, text or when it comes back, or resolve it (with why).

### `update_screenshot`
Change a screenshot: a mockup's state (proposed, approved, built), its design link, name or
description. Upload with `flow screenshot`.

## Records

### `get_record`
One record in full: fields, the owner's words or answer, links, and its history.

### `find_records`
Find records by project (or `"all"`), kind, state, milestone, tag, words, or changed since a date.
`include_closed` and `include_archived` widen it; `compact` for long lists.

### `link`
Link two records: `supersedes`, `answers`, `blocks`, `implements`, `records`, `depends_on`,
`feedback_on`, `relates`.

## Contributions

### `draft_contribution`
Draft an issue for someone else's repository, or a comment on a PR or issue already sent. The owner
reads it, may edit it, and presses Send. PRs and new commits use `flow contribute`.

### `track_contribution`
Watch a PR or issue sent before AgentFlow sent them: checked every 30 minutes for comments,
reviews, merges and closes.

## The owner

### `notify_owner`
An urgent notification for something that can't wait: a broken release, a full disk, a blocked
stack. Questions go through `ask_question`.

### `handbook`, `handbook_read` and `handbook_edit`
The owner's handbook: how they work, the machine, procedures. `handbook` lists its files and
sections; `handbook_read` reads one, or one section, with its version. Only the consultant edits it
(`handbook_edit`, with that version), and every edit is a revision.
