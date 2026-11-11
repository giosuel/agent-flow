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

`scripts/install.sh` builds a release binary, installs it as `~/.local/bin/agentflow` and runs it as
a systemd user service (`packaging/agentflow.service`).

## Configuration

`~/.config/agentflow/config.toml`; every field is optional:

```toml
listen = "127.0.0.1:8790"            # never a wildcard address; refused at start-up
public_url = "https://flow.example"  # decides cookie flags, passkey domain and allowed hosts
data_dir = "~/.local/state/agentflow"
trusted_proxies = ["127.0.0.1", "::1"]  # may set X-Forwarded-For (a reverse proxy)
session_idle_days = 14
session_max_days = 30
```

## Access

Every request is authenticated, nothing is public but the sign-in page:

- **The owner** signs in in the browser. On a new device, run `agentflow admin login-link` on the
  server and open the one-time link it prints (valid 15 minutes).
- **Tokens** for everything else: `agentflow admin token create --kind device|project|admin
  --name … [--project …] [--out FILE]`. A project token may change its own project and read the
  rest; an admin token (the consultant) may change anything. Only the owner's browser session can
  press merge and tag buttons. `agentflow admin token list` and `… revoke ID` manage them.

## Connecting an agent

Register the MCP endpoint with Claude Code and let it fetch the token itself, so the token never
sits in `~/.claude.json`:

```sh
claude mcp add-json agentflow '{"type": "http", "url": "https://flow.example/mcp",
  "headersHelper": "agentflow mcp-headers --token-file ~/.config/agentflow/tokens/<project>.token"}'
```

## Development

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Versions come from the git tags (`scripts/version.sh`, `version.rs`), like MinVer.

## License

AGPL-3.0-or-later. The IBM Plex fonts in `static/fonts/` are under the SIL Open Font License
(`static/fonts/OFL.txt`); htmx (`static/htmx.min.js`) is under the BSD Zero Clause License.
