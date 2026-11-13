#!/usr/bin/env bash
# Builds the two Debian packages from the release programs (the owner, 2026-11-13: "the server
# package will be called agentflow and the client agentflow-client"):
#   target/agentflow_<version>_<arch>.deb         the server: agentflow, its system service, the
#                                                  agents' helper service and the agentflow user
#   target/agentflow-client_<version>_<arch>.deb  flow, and agentflow-builder with its user service
# On the server:
#   sudo apt install ./agentflow-client_<version>_<arch>.deb ./agentflow_<version>_<arch>.deb
#   sudo agentflow setup --public-url https://flow.example --agents-user <user>   (see README)
# On other machines only the client. Needs dpkg-deb only. Build the programs on the oldest
# distribution you support: they need at least that glibc.
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/agentflow-target}"
rel="${REL:-$CARGO_TARGET_DIR/release}"
for b in agentflow flow agentflow-builder; do
  [ -x "$rel/$b" ] || { echo "no release program $rel/$b; build them first (scripts/install.sh builds them)" >&2; exit 1; }
done
version=$(scripts/version.sh)
arch=$(dpkg --print-architecture)
glibc=$(ldd --version | head -1 | grep -oE '[0-9]+\.[0-9]+$')
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The libraries a program links, as the packages that hold them here; glibc at least the one it
# was built with.
deps_of() {
  local deps="libc6 (>= $glibc)" lib pkg
  for lib in $(ldd "$1" | awk '/=> \//{print $3}'); do
    pkg=$(dpkg -S "$(readlink -f "$lib")" 2>/dev/null | head -1 | cut -d: -f1) || true
    case "$pkg" in "" | libc6) ;; *) [[ "$deps" == *"$pkg"* ]] || deps="$deps, $pkg" ;; esac
  done
  echo "$deps"
}

# ---- agentflow-client: flow and agentflow-builder
client="$work/client"
mkdir -p "$client"
chmod 755 "$client"
install -Dm755 "$rel/flow" "$client/usr/bin/flow"
install -Dm755 "$rel/agentflow-builder" "$client/usr/bin/agentflow-builder"
sed 's|@BIN@|/usr/bin/agentflow-builder|' packaging/system/agentflow-builder.service |
  install -Dm644 /dev/stdin "$client/usr/lib/systemd/user/agentflow-builder.service"
install -Dm644 LICENSE "$client/usr/share/doc/agentflow-client/copyright"
mkdir -p "$client/DEBIAN"
cat >"$client/DEBIAN/control" <<CONTROL
Package: agentflow-client
Version: $version
Architecture: $arch
Maintainer: giosuel <me@giosuel.ch>
Depends: $(deps_of "$rel/flow"), git
Section: devel
Priority: optional
Homepage: https://github.com/giosuel/agent-flow
Installed-Size: $(du -sk "$client/usr" | cut -f1)
Description: AgentFlow's command, flow, and its builder
 flow connects a machine to an AgentFlow server (flow connect), runs test builds
 (flow run), sets projects up (flow project setup) and makes the machine a builder
 for agents' builds and tests (flow builder add, with agentflow-builder).
CONTROL
cat >"$client/DEBIAN/postinst" <<'POSTINST'
#!/bin/sh
set -e
if [ "$1" = configure ]; then
  echo "flow is installed. Connect it to your AgentFlow server: flow connect https://…"
  echo "To make this machine a builder: flow builder add. Its service keeps running while its user"
  echo "isn't logged in only with linger on, once: sudo loginctl enable-linger <user>"
fi
POSTINST
chmod 755 "$client/DEBIAN/postinst"

# ---- agentflow: the server
server="$work/server"
mkdir -p "$server"
chmod 755 "$server"
install -Dm755 "$rel/agentflow" "$server/usr/bin/agentflow"
sed 's|@BIN@|/usr/bin/agentflow|' packaging/system/agentflow.service |
  install -Dm644 /dev/stdin "$server/usr/lib/systemd/system/agentflow.service"
sed 's|@BIN@|/usr/bin/agentflow|' packaging/system/agentflow-hands.service |
  install -Dm644 /dev/stdin "$server/usr/lib/systemd/user/agentflow-hands.service"
install -Dm644 packaging/system/agentflow.sysusers "$server/usr/lib/sysusers.d/agentflow.conf"
install -Dm644 LICENSE "$server/usr/share/doc/agentflow/copyright"
install -Dm644 README.md "$server/usr/share/doc/agentflow/README.md"
mkdir -p "$server/DEBIAN"
cat >"$server/DEBIAN/control" <<CONTROL
Package: agentflow
Version: $version
Architecture: $arch
Maintainer: giosuel <me@giosuel.ch>
Depends: $(deps_of "$rel/agentflow"), agentflow-client (= $version), systemd, git
Recommends: gh
Section: devel
Priority: optional
Homepage: https://github.com/giosuel/agent-flow
Installed-Size: $(du -sk "$server/usr" | cut -f1)
Description: Project management for Claude agents and their owner
 Records, questions, pull requests, builds and a log for Claude Code agents, with a
 web page for their owner. The server runs as its own user; set it up with:
 sudo agentflow setup --public-url https://… --agents-user <user>
CONTROL
cat >"$server/DEBIAN/postinst" <<'POSTINST'
#!/bin/sh
set -e
# The user exists from the start; `agentflow setup` does the rest.
if [ "$1" = configure ]; then
  if command -v systemd-sysusers >/dev/null; then
    systemd-sysusers agentflow.conf
  fi
  echo "Next: sudo agentflow setup --public-url https://… --agents-user <user>"
  echo "Then your first passkey: sudo flow server add-passkey (or the link in journalctl -u agentflow)"
fi
POSTINST
chmod 755 "$server/DEBIAN/postinst"

mkdir -p target
for p in client:agentflow-client server:agentflow; do
  dir="$work/${p%%:*}"
  out="target/${p#*:}_${version}_${arch}.deb"
  dpkg-deb --root-owner-group -Zzstd --build "$dir" "$out" >/dev/null
  echo "$out"
done
