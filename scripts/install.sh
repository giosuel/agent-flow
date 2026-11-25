#!/usr/bin/env bash
# Builds the release programs (on the builder when it can), installs the server as ~/.local/bin/agentflow and the client as
# ~/.local/bin/flow, and (re)starts the user service.
# Run from a checkout on the server. Uses its own target dir and a memory cap (shared machine).
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/agentflow-target}"
# The programs are built on the project's builder when it's online (the owner, 2026-11-17: "lets
# do 3 first as highest prio"): portable ones, for glibc 2.35 and newer with OpenSSL compiled in
# (cargo-zigbuild), so a builder with a newer system builds programs this machine runs. That needs
# `flow` with this project's token (AGENTFLOW_TOKEN_FILE). Otherwise, or with LOCAL=1, they're
# built here as before.
T=x86_64-unknown-linux-gnu
rel=""
if [ -z "${LOCAL:-}" ] && [ -n "${AGENTFLOW_TOKEN_FILE:-}" ] && command -v flow >/dev/null; then
  # Every job has a name (D-275); a flow from before names doesn't know --name yet.
  name=()
  if flow remote --help 2>/dev/null | grep -q -- '--name'; then name=(--name "AgentFlow for a live install"); fi
  set +e
  flow remote "${name[@]}" --fetch "target/$T/release/agentflow" --fetch "target/$T/release/flow" --fetch "target/$T/release/agentflow-builder" \
    -- cargo zigbuild --release --locked --features vendored-openssl --target "$T.2.35"
  code=$?
  set -e
  case $code in
    0) rel="target/$T/release" ;;
    75 | 78) echo "no builder for this project online; building here" >&2 ;;
    *) echo "the build on the builder failed (exit $code); nothing was installed" >&2; exit "$code" ;;
  esac
fi
if [ -z "$rel" ]; then
  systemd-run --user --scope -q -p MemoryMax=6G -p MemorySwapMax=0 \
    cargo build --release --locked -j "${JOBS:-2}"
  rel="$CARGO_TARGET_DIR/release"
fi
for bin in agentflow flow agentflow-builder; do
  "$rel/$bin" --version >/dev/null || { echo "$rel/$bin doesn't start here; nothing was installed" >&2; exit 1; }
done
install -d "$HOME/.local/bin" "$HOME/.config/systemd/user"
# A backup before anything changes, by the new program (it opens the database read-only, so it
# never migrates it; the old server keeps running until the restart below).
if [ -n "${AGENTFLOW_CONFIG:-}" ] || [ -f "$HOME/.config/agentflow/config.toml" ]; then
  "$rel/agentflow" backup --name "pre-install-$(scripts/version.sh 2>/dev/null | tr -c 'A-Za-z0-9.\n-' '-' | tail -c 40)" ||
    { echo "the backup failed; nothing was installed" >&2; exit 1; }
fi
# Copy next to the old binary, then rename: a running server keeps its file until it restarts.
for bin in agentflow flow agentflow-builder; do
  install -m 755 "$rel/$bin" "$HOME/.local/bin/.$bin.new"
  mv "$HOME/.local/bin/.$bin.new" "$HOME/.local/bin/$bin"
done
# flow-run was the same program by another name; `flow run` replaces it.
if [ -L "$HOME/.local/bin/flow-run" ]; then rm "$HOME/.local/bin/flow-run"; fi
install -m 644 packaging/agentflow.service "$HOME/.config/systemd/user/agentflow.service"
systemctl --user daemon-reload
systemctl --user enable agentflow.service >/dev/null
systemctl --user restart agentflow.service
sleep 1
systemctl --user --no-pager --lines=5 status agentflow.service
