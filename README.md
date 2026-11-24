# AgentFlow

Project management for a team of Claude agents and the person they work for. Each project's agent
records its tasks, decisions, questions, pull requests and builds in AgentFlow over MCP; the owner
answers questions, merges, tags and gives feedback from a web page that also works as a home-screen
app on the phone. Everything that happens is logged.

One Rust binary holds the server, the web app, the MCP endpoint and the command-line tools. Data
lives in SQLite in `~/.local/state/agentflow/`.

## Build and run

```sh
cargo build --release
./target/release/agentflow serve
```

There are two ways to run it.

**As its own user (recommended on a shared machine).** The agents can't read AgentFlow's
database, tokens or sessions. Install the binary where only root can change it, then run setup once:

```sh
sudo install -m 755 target/release/agentflow /usr/local/bin/agentflow
sudo /usr/local/bin/agentflow setup --public-url https://flow.example --agents-user <user> --dry-run  # the steps
sudo /usr/local/bin/agentflow setup --public-url https://flow.example --agents-user <user>
# then, as the agents' user:
systemctl --user daemon-reload; systemctl --user enable --now agentflow-hands
```

Setup creates the user `agentflow` and writes `/etc/agentflow/config.toml`. It installs the
system service (`packaging/system/agentflow.service`, data in `/var/lib/agentflow`, no access to
`/home`) and `agentflow-hands`, a user service of the agents' user. `--import <dir>` brings
a single-user instance's data along.

The server runs as `agentflow`; what has to happen as the agents' user goes to `agentflow hands`
over a Unix socket that only `agentflow` may use (`src/hands.rs`). That covers the buttons, `gh`
with the agents' login, guide edits, token files, new projects' folders, transcripts and the
agents' process folders. Admin commands then run as the server's user:
`sudo -u agentflow agentflow --config /etc/agentflow/config.toml admin login-link`.

**Packages.** `scripts/deb.sh` makes a `.deb` from a release binary, and `packaging/arch/PKGBUILD`
builds the AUR package from a release tag. Both install the binary, the two services and the user;
then run `sudo agentflow setup …` as above (no `install` step needed).

**As one user (a personal machine).** `scripts/install.sh` builds a release binary, installs it as
`~/.local/bin/agentflow` and runs it as a systemd user service (`packaging/agentflow.service`).
Everything runs as you, so agents running as you can read its files.

## Configuration

`~/.config/agentflow/config.toml`; every field is optional:

```toml
listen = "127.0.0.1:8790"            # never a wildcard address; refused at start-up
public_url = "https://flow.example"  # decides cookie flags, passkey domain and allowed hosts
data_dir = "~/.local/state/agentflow"
trusted_proxies = ["127.0.0.1", "::1"]  # may set X-Forwarded-For (a reverse proxy)
session_idle_days = 14
session_max_days = 30
token_dir = "~/.config/agentflow/tokens"   # project tokens for headersHelper
push_contact = "mailto:you@example.com"     # the sender contact push services ask for
handbook_dir = "~/ai-handbook"              # the guides: a git checkout agents read through AgentFlow
handbook_repo = "owner/ai-handbook"         # its GitHub repository, for pull requests
git_author = "Name <you@example.com>"       # who your guide edits are committed as
github_writes = true                        # false on a test instance: no merges on GitHub
build_free_gb = 10                          # disk a build upload must leave free
transcripts_dir = "~/.claude/projects"      # Claude Code transcripts, read for token usage
agents_home = "/home/agents"                # as its own user: where ~/ points (the agents' home)
hands_socket = "/run/agentflow-hands/hands.sock"  # as its own user: the agents' helper
external_token_file = "~/.config/gh/external-token"  # public_repo token; used only on the owner's Send
git_host = "https://github.com"             # where drafts' repositories are fetched and pushed

[[build_servers]]                           # machines agents build and test on (agentflow remote)
name = "headquarters"
ssh = "headquarters"                        # a Host from ~/.ssh/config, or user@host
root = "agentflow"                          # its work folders, in that user's home
min_threads = 8                             # another job starts while this many threads…
min_memory_gb = 16                          # …and this much memory are free
reserve_memory_gb = 8                       # below this, running jobs pause until there's room
keep_days = 14                              # a project's folder unused this long is deleted
```

## Behind Caddy (passkeys and push)

Passkeys and push notifications need https on a domain name. With Caddy in front:

```
flow.example {
    reverse_proxy 127.0.0.1:8790
}
```

and in `config.toml`: `listen = "127.0.0.1:8790"`, `public_url = "https://flow.example"`. On a
plain-http address (e.g. a VPN IP) everything else works and sign-in uses one-time links.

## Home-screen app and notifications

AgentFlow is a web app you can add to the home screen (on iPhone: Share, then Add to Home Screen;
iOS 16.4 or later). Open it from there, go to Settings and press "Turn on for this device". Push
messages are encrypted for each device (RFC 8291) and signed with the server's VAPID key
(`vapid.key` in the data directory). Settings has a switch for each kind: new question, build
ready to try, something to review, and urgent calls from an agent.

## Access

Every request is authenticated, nothing is public but the sign-in page:

- **The owner** signs in in the browser with a passkey (enrolled in Settings). On a new device, or
  if every passkey is lost, run `agentflow admin login-link` on the server and open the one-time
  link it prints (valid 15 minutes).
- **Tokens** for everything else: `agentflow admin token create --kind device|project|admin
  --name … [--project …] [--out FILE]`. A project token may change its own project and read the
  rest; an admin token (the consultant) may change anything. Only the owner's browser session can
  press merge and tag buttons. `agentflow admin token list` and `… revoke ID` manage them, and so
  does Settings, where project and consultant tokens can be rotated: the new token is written to
  `~/.config/agentflow/tokens/<project>.token` for the agent's `headersHelper`.

## Connecting an agent

**New project** on the Projects page creates the project and an empty M0, its folder with a local
git repository (an existing folder or repository is left alone, so it onboards existing projects
too), a project token in `~/.config/agentflow/tokens/<project>.token`, and a first note in the
agent's memory. It then shows the command that registers AgentFlow with Claude Code for that
folder, letting Claude fetch the token itself so it never sits in `~/.claude.json`:

```sh
claude mcp add-json agentflow '{"type": "http", "url": "https://flow.example/mcp",
  "headersHelper": "agentflow mcp-headers --token-file ~/.config/agentflow/tokens/<project>.token"}'
```

Start the agent in that folder with `env AGENTFLOW_TOKEN_FILE=~/.config/agentflow/tokens/<project>.token
claude` (the page shows this command too), so the `agentflow` commands it runs (publish, screenshot,
contribute, remote, wait) use the same token. `hello` tells the agent where the file is as well.

`agentflow admin managed-settings` prints Claude Code managed settings that keep every session's
Read and Edit tools away from AgentFlow's tokens, data and config, and the root command to
install them. Shell commands run as the same user can still read those files; running AgentFlow
as its own Unix user is the stronger fence.

## Builds

Agents publish test builds with `agentflow publish <file-or-folder> --product <name> --summary …
[--entry …] [--feature "Title: what to try"]…`; the owner runs them on any machine with
`flow-run <product> [number]` (the same program, linked as `flow-run`), which downloads the build once, checks every file against
its checksum and starts it. A build with features waits for feedback on its page.

## Screenshots

Agents upload screenshots with `agentflow screenshot <file> --name "…" --category "…"
[--description "…"] [--record T-12]`: PNG, JPEG or WebP up to 10 MB. The category is free text
(test build, mockup, spike test, bug, …). Each one is a record (`S-4`) on the project's Screenshots
tab, which can filter by category, and opens full size on its own page. With `--record` it's linked
to that task, build or question and listed there too. The images are in `<data_dir>/screenshots/`.

## Other people's repositories

Agents never post or push to other people's repositories themselves. They draft: `agentflow
contribute pr --repo <owner/name> --base main --branch <name> --title "…" --body-file <file>` in
their checkout sends the commits on top of that branch (as a git bundle), `agentflow contribute
issue|comment|commits` the rest, and the MCP tool `draft_contribution` does issues and comments.
AgentFlow reads the commits into a repository of its own (`<data_dir>/outgoing/`) and shows the
owner the diff it computed, the text and a few checks (their PR template's headings, Claude
attribution, a linked decision). The owner can edit the text, then press **Send** (AgentFlow pushes
to the owner's fork and opens the PR, issue or comment as them, with exactly that text), **Ask for
changes** (the note goes to the agent word for word), or **Drop**. Sent ones are watched every 30
minutes; new comments, reviews, merges and closes go to the project's agent.

Sending needs `external_token_file`: a GitHub token with `public_repo`, read only when the owner
presses Send and never shown or logged.

## Build servers

Agents run builds and tests on a stronger machine with `agentflow remote -- cargo build --release`
(`--fetch target/release/app` brings files back, `--gpu` waits for an idle GPU). AgentFlow does the
work over ssh. Each project has one work folder there: the commit goes over with git (only new
objects), then every file is compared by content with rsync, so only what changed is sent and
rewritten, and cargo rebuilds only that. The target folder stays there between jobs; sccache is used
when it's installed there. Jobs run in a systemd unit of their own at low CPU and IO weight: they
take what the machine's user isn't using, several at once when there's room, and pause when memory
runs short. Output streams to the agent, the job's page and the Log (`via:build`; the job's entry
has its warnings and errors). When the server is offline, the command exits with 75 at once, so
the agent builds locally.

Build servers are managed on the Settings page: **Add** one by name and `user@host` (or a Host
from `~/.ssh/config`); AgentFlow can make a key for it and shows the one line to add to that user's
`authorized_keys` (with `restrict`). **Check** shows whether it has Rust and sccache, **Set up
Rust** and **Install sccache** do that in the background, and **Limits** sets when jobs start and
pause. Each project picks its build server in its own Settings; a project without one builds
where its agent is (`agentflow remote` exits with 75). Servers in `config.toml` are added at start.

For the low priority to hold against the machine's own user, give the build user's slice a low
weight once (as root there): `systemctl set-property user-<uid>.slice CPUWeight=10 IOWeight=10`.

## Guides

The guides are the Markdown files of a git repository (`handbook_dir`). Agents read them through
MCP (`guides`, `guide`). The Guides page shows them by section; your edits are committed to the
checkout's main branch as `git_author` and never pushed. The consultant changes them with pull
requests, which the page lists with a Merge button pinned to the commit you saw.

## Usage and Machine

Every five minutes AgentFlow reads what's new in Claude Code's transcripts and keeps one row per
model call (keyed by its message id, so subagents' repeats count once): the Usage page shows
tokens per day or hour, per agent, per project and per task. Claude Code deletes transcripts
after 30 days; the counts stay. The Machine page samples the machine every minute and puts each
process under the agent session that started it.

## Backups

Once a day the database is copied to `<data_dir>/backups/flow-<date>.db` (the last 30 are kept)
and every project is exported to `<data_dir>/export/<slug>.md`. `agentflow admin backup` does
both now. Reads older than 90 days move to monthly compressed archives in `<data_dir>/log-archive/`;
nothing in the log is ever deleted.

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Versions come from the git tags (`scripts/version.sh`, `version.rs`), like MinVer.

## License

AGPL-3.0-or-later. The IBM Plex fonts in `static/fonts/` are under the SIL Open Font License
(`static/fonts/OFL.txt`).
