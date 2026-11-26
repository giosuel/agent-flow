---
title: Build servers
description: Machines that connect themselves and run agents' builds and tests with flow remote.
---

Agents run heavy builds and tests on a stronger machine:

```sh
flow remote --name "Release build" -- cargo build --release
flow remote --name "Lumen for a test build" --fetch target/release/lumen -- cargo build --release
flow remote --name "Tests for PR #12" -- cargo test --workspace
```

Every job has a name, a few words on one line: it's the job's title in AgentFlow.

The output streams back as it comes, and `flow remote` exits with the command's exit code.

| Exit code | Meaning |
| --- | --- |
| `75` | The build server is offline: build here instead. |
| `78` | No build server is picked for the project; the message names the online ones for `--server`. |

## Adding one

A build server is a builder: `agentflow-builder`, a background program that connects out to the
server over a WebSocket. Nothing on it accepts connections, so it can sit behind any NAT; it's
online exactly while it's connected. On the machine, with `flow` connected:

```sh
flow builder add
```

It asks whether the builder gets a user of its own (recommended, so projects' build scripts and
tests never see your files), its name, and your approval in the browser; then it starts its user
service. A builder keeps running while its user isn't logged in only with linger on:
`sudo loginctl enable-linger <user>`. One builder per user.

Each project picks its build server in its own Settings. A project without one builds where its
agent is.

## How a job runs

- Each project has a work folder on the builder. `flow remote` sends a snapshot of the checkout:
  only files the server and the builder don't have yet travel, by content, zstd-compressed and
  checked by SHA-256, with new commits as a git bundle.
- Only files that changed are rewritten, so the build tool rebuilds only that. The target folder
  stays between jobs, and sccache is used when it's installed there.
- Jobs run in a systemd unit of their own at low CPU and IO weight: they take what the machine's
  user isn't using, run several at once when there's room, and pause when memory runs short.
- A job keeps running when the network drops; its output and end are sent when the builder is
  back. `flow remote --cancel 338`, or Ctrl-C, stops it with all its processes.
- Output streams to the agent, the job's page and the Log.

| Option | What it does |
| --- | --- |
| `--name TEXT` | The job's name, its title in AgentFlow (required). |
| `--server NAME` | Which build server, if not the project's. |
| `--fetch PATH` | Bring a file or folder back when it succeeds (repeat for more). `target/…` comes from the project's target folder there. |
| `--gpu` | Wait until the GPU there is idle. |
| `--dir DIR` | Run it in a folder of the checkout. |
| `--setup` | Run it in the builder's home instead, to install the tools a project needs: `flow remote --setup --name "Install the tools" -- sh -c '…'`. |
| `--cancel JOB` | Cancel jobs of this project by number. |

## Limits

`flow builder edit [name]` changes a builder's limits; the Settings page shows them.

| Limit | Default | Meaning |
| --- | --- | --- |
| `--min-threads` | 8 | Start another job only while at least this many threads are free… |
| `--min-memory` | 16 GB | …and this much memory. |
| `--reserve-memory` | 8 GB | Below this much free memory, running jobs pause. |
| `--keep-days` | 14 | A project's work folder unused this long is deleted. |
| `--slots` | 1 | How many jobs of one project run at once, each in a folder of its own (1–8). |

For the low priority to hold against the machine's own user, give the build user's slice a low
weight once, as root there:

```sh
systemctl set-property user-<uid>.slice CPUWeight=10 IOWeight=10
```

## Watching

`flow builder status [name]` follows a builder live in the terminal: its CPU (yours and the
builds'), memory, GPU and disk, its jobs, and the output of the one you pick. `↑↓` picks a job, `⏎`
follows it, `p` pauses the builds there, `q` quits. The web app's **Machine** page shows the same,
with a page per job. `flow builder list` and `flow builder remove` do the rest.
