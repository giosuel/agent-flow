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
token_dir = "~/.config/agentflow/tokens"   # project tokens for headersHelper
push_contact = "mailto:you@example.com"     # the sender contact push services ask for
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
(`static/fonts/OFL.txt`).
