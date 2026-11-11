#!/usr/bin/env bash
# Builds a release binary, installs it as ~/.local/bin/agentflow and (re)starts the user service.
# Run from a checkout on the server. Uses its own target dir and a memory cap (shared machine).
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/agentflow-target}"
systemd-run --user --scope -q -p MemoryMax=6G -p MemorySwapMax=0 \
  cargo build --release --locked -j "${JOBS:-2}"
install -d "$HOME/.local/bin" "$HOME/.config/systemd/user"
# Copy next to the old binary, then rename: a running server keeps its file until it restarts.
install -m 755 "$CARGO_TARGET_DIR/release/agentflow" "$HOME/.local/bin/.agentflow.new"
mv "$HOME/.local/bin/.agentflow.new" "$HOME/.local/bin/agentflow"
install -m 644 packaging/agentflow.service "$HOME/.config/systemd/user/agentflow.service"
systemctl --user daemon-reload
systemctl --user enable agentflow.service >/dev/null
systemctl --user restart agentflow.service
sleep 1
systemctl --user --no-pager --lines=5 status agentflow.service
