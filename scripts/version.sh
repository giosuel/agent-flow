#!/usr/bin/env bash
# Prints the version of the checked-out commit, worked out from the v* tags (like MinVer):
#   on a tag v0.4.0          →  0.4.0
#   5 commits after v0.4.0   →  0.4.1-dev.5+gde73aff
#   5 commits after v0.5.0-rc.1  →  0.5.0-rc.1.5+gde73aff
#   no tag yet               →  0.0.0-dev+gde73aff
# crates/version.rs (or build.rs) does the same for local builds; keep the two in step.
set -euo pipefail
cd "$(dirname "$0")/.."
hash=$(git rev-parse --short=7 HEAD)
if ! described=$(git describe --tags --match 'v[0-9]*' --long --abbrev=7 2>/dev/null); then
  echo "0.0.0-dev+g$hash"
  exit
fi
# v0.4.0-5-gde73aff (the tag itself may contain dashes: v0.5.0-rc.1-5-gde73aff)
height=${described%-g*}
height=${height##*-}
tag=${described%-*-g*}
version=${tag#v}
if [ "$height" = 0 ]; then
  echo "$version"
elif [[ "$version" == *-* ]]; then
  echo "$version.$height+g$hash"
else
  IFS=. read -r major minor patch <<<"$version"
  echo "$major.$minor.$((patch + 1))-dev.$height+g$hash"
fi
