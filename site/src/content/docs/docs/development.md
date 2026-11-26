---
title: Development
description: Building, testing and the layout of the repository.
---

AgentFlow is one Rust crate (edition 2024, Rust 1.90 or newer) with three programs. The checks CI
runs:

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

The tests include end-to-end tests of the HTTP layer: a real server on a random local port, real
requests (`src/web/tests.rs`).

## Layout

| Path | What's there |
| --- | --- |
| `src/main.rs` | `agentflow`, the server. |
| `src/bin/flow.rs` | `flow`. |
| `src/bin/agentflow-builder.rs` | The builder. |
| `src/mcp/` | The MCP tools and the server instructions. |
| `src/web/` | The HTTP server: pages, the JSON API, the MCP endpoint. |
| `templates/` | The pages (askama). |
| `static/` | Styles, scripts, fonts, icons, compiled into the binary. |
| `migrations/` | The SQLite schema. |
| `packaging/`, `scripts/` | systemd units, the AUR `PKGBUILD`, `install.sh`, `deb.sh`, `version.sh`. |
| `site/` | This website. |

## The UI library

`static/ui.css` holds every token and component once; pages build only from those. `/ui` on a
running server shows the catalogue: every token and component in its states, drawn with the
stylesheet itself. This website uses the same file.

## Versions

Versions come from the git tags, like MinVer (`scripts/version.sh`, `version.rs`):

| Checkout | Version |
| --- | --- |
| on `v0.4.0` | `0.4.0` |
| 5 commits after `v0.4.0` | `0.4.1-dev.5+gde73aff` |
| no tag yet | `0.0.0-dev+gde73aff` |

`cargo build --release --features vendored-openssl` builds programs with OpenSSL compiled in, so
they need no particular OpenSSL on the machine they run on.

## Packages

`scripts/deb.sh` builds the two `.deb` packages from the release programs.
`packaging/arch/PKGBUILD` builds the AUR split package from a release tag.

## This website

```sh
cd site
npm ci
npm run dev       # http://localhost:4321/agent-flow/
npm run build     # into site/dist
```

`SITE_URL` and `SITE_BASE` say where it's served (default: `https://giosuel.github.io/agent-flow/`).
The Pages workflow builds and deploys it on every push to `main`.
