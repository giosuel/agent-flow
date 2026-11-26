---
title: Questions and decisions
description: The agent asks, the owner answers, and the answer becomes a decision with their words.
---

## Asking

When the agent needs the owner, it calls `ask_question`: one question per call, with the context
needed to answer without opening anything else, numbered options and its recommendation. It asks
in chat too, with the question's id (`Q-12`), so the owner can answer in either place.

| Option | What it does |
| --- | --- |
| `scope` | Where the answer applies: a milestone (the current one by default) or `"project"`. |
| `blocks` | Tasks that wait for the answer. They're set blocked and come back by themselves. |
| `going_ahead` | The choice is cheap to change: the agent builds its recommendation meanwhile, and the owner confirms or picks another later. |
| `related` | Earlier decisions the question touches, shown beside it. |
| `in_chat` | The owner is talking with the agent right now: recorded, but no notification. |
| `urgent` | Sent as an urgent notification. Only when waiting would really cost something. |

The agent never stops for a question: it keeps working on whatever doesn't depend on it.

## Answering

Open questions lead the Dashboard and the project's **Questions** tab. Each option has its row,
the recommended one marked; pick one or answer in your own words, then **Send answer**. The owner
gets a push notification for each new question, unless it was asked in chat.

## Decisions

An answer becomes a decision in the question's scope by itself, quoting the owner's words, and the
agent hears of it in its news. An answer given in chat, the agent records with `answer_question`.

Anything else the owner decides, the agent records with `record_decision`: one statement, why,
the owner's words verbatim, when they said them, and where they came from. A decision can replace
an older one; the replaced one folds under it. `update_decision` re-scopes, corrects, replaces or
archives decisions, keeping the old text in the history.

The **Questions** tab shows the open questions first, then every decision as a timeline by day,
filtered by words, scope, state and time.

The agent sees project-wide decisions and those of milestones that aren't closed. `find_records`
with `include_closed` reaches the rest. ADRs that live in a repository are linked with `add_adr`.

## Hearing the news

The owner's answers, button presses and feedback reach the agent four ways:

- in the reply of its next write call (marked read);
- with `since_last_check`;
- `flow news`, which a hook runs before each of the owner's prompts;
- `flow wait`, run in the background: it exits when something happens, which wakes an idle session.
