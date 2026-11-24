#!/usr/bin/env bash
# Builds target/agentflow_<version>_<arch>.deb from a release binary: the binary, the system service
# (as its own user), the agents' helper service and the agentflow user. Then, on the server:
#   sudo apt install ./agentflow_<version>_<arch>.deb
#   sudo agentflow setup --public-url https://flow.example --agents-user <user>   (see README)
# Needs dpkg-deb only. Build the binary on the oldest distribution you support: it needs at
# least that glibc.
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.cache/agentflow-target}"
bin="${BIN:-$CARGO_TARGET_DIR/release/agentflow}"
[ -x "$bin" ] || { echo "no release binary at $bin; build it first (scripts/install.sh builds one)" >&2; exit 1; }
version=$(scripts/version.sh)
arch=$(dpkg --print-architecture)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
chmod 755 "$stage"

install -Dm755 "$bin" "$stage/usr/bin/agentflow"
ln -s agentflow "$stage/usr/bin/flow-run"
sed 's|@BIN@|/usr/bin/agentflow|' packaging/system/agentflow.service >"$stage/agentflow.service"
sed 's|@BIN@|/usr/bin/agentflow|' packaging/system/agentflow-hands.service >"$stage/agentflow-hands.service"
install -Dm644 "$stage/agentflow.service" "$stage/usr/lib/systemd/system/agentflow.service"
install -Dm644 "$stage/agentflow-hands.service" "$stage/usr/lib/systemd/user/agentflow-hands.service"
rm "$stage/agentflow.service" "$stage/agentflow-hands.service"
install -Dm644 packaging/system/agentflow.sysusers "$stage/usr/lib/sysusers.d/agentflow.conf"
install -Dm644 LICENSE "$stage/usr/share/doc/agentflow/copyright"
install -Dm644 README.md "$stage/usr/share/doc/agentflow/README.md"

# The libraries it links, as the packages that hold them here; glibc at least the one it was built with.
glibc=$(ldd --version | head -1 | grep -oE '[0-9]+\.[0-9]+$')
deps="libc6 (>= $glibc)"
for lib in $(ldd "$bin" | awk '/=> \//{print $3}'); do
  pkg=$(dpkg -S "$(readlink -f "$lib")" 2>/dev/null | head -1 | cut -d: -f1) || true
  case "$pkg" in "" | libc6) ;; *) [[ "$deps" == *"$pkg"* ]] || deps="$deps, $pkg" ;; esac
done

mkdir -p "$stage/DEBIAN"
cat >"$stage/DEBIAN/control" <<CONTROL
Package: agentflow
Version: $version
Architecture: $arch
Maintainer: giosuel <me@giosuel.ch>
Depends: $deps, systemd, git
Recommends: gh
Section: devel
Priority: optional
Homepage: https://github.com/giosuel/agent-flow
Installed-Size: $(du -sk "$stage/usr" | cut -f1)
Description: Project management for Claude agents and their owner
 Records, questions, pull requests, builds and a log for the Claude Code agents
 on a machine, with a web page for their owner. Runs as its own user; set it up
 with: sudo agentflow setup --public-url https://… --agents-user <user>
CONTROL
cat >"$stage/DEBIAN/postinst" <<'POSTINST'
#!/bin/sh
set -e
# The user exists from the start; `agentflow setup` does the rest.
if [ "$1" = configure ] && command -v systemd-sysusers >/dev/null; then
  systemd-sysusers agentflow.conf
fi
POSTINST
chmod 755 "$stage/DEBIAN/postinst"
out="target/agentflow_${version}_${arch}.deb"
mkdir -p target
dpkg-deb --root-owner-group -Zzstd --build "$stage" "$out" >/dev/null
echo "$out"
