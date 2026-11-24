#!/usr/bin/env bash
# Builds the release programs, installs the server as ~/.local/bin/agentflow and the client as
# ~/.local/bin/flow, and (re)starts the user service.
# Run from a checkout on the server. Uses its own target dir and a memory cap (shared machine).
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/agentflow-target}"
systemd-run --user --scope -q -p MemoryMax=6G -p MemorySwapMax=0 \
  cargo build --release --locked -j "${JOBS:-2}"
install -d "$HOME/.local/bin" "$HOME/.config/systemd/user"
# Copy next to the old binary, then rename: a running server keeps its file until it restarts.
for bin in agentflow flow; do
  install -m 755 "$CARGO_TARGET_DIR/release/$bin" "$HOME/.local/bin/.$bin.new"
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
