#!/usr/bin/env bash
# Renders the home-screen icons in static/icons/ from static/logo.svg (needs rsvg-convert).
# The logo sits on black: iOS shows transparent icons on a white square.
set -euo pipefail
cd "$(dirname "$0")/.."
inner=$(sed -e '1d' -e '$d' -e 's|<title>.*</title>||' static/logo.svg)
icon() { # <scale> <out.svg>
  local off; off=$(awk -v s="$1" 'BEGIN { printf "%.3f", (1 - s) * 32 }')
  printf '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" fill="none" stroke-linecap="round" stroke-linejoin="round"><rect width="64" height="64" fill="#000"/><g transform="translate(%s %s) scale(%s)">%s</g></svg>' "$off" "$off" "$1" "$inner" > "$2"
}
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
icon 0.86 "$tmp/full.svg"
icon 0.68 "$tmp/mask.svg"   # maskable: the logo stays inside the 80% safe zone
rsvg-convert -w 180 -h 180 "$tmp/full.svg" -o static/icons/apple-touch-icon.png
rsvg-convert -w 192 -h 192 "$tmp/full.svg" -o static/icons/icon-192.png
rsvg-convert -w 512 -h 512 "$tmp/full.svg" -o static/icons/icon-512.png
rsvg-convert -w 512 -h 512 "$tmp/mask.svg" -o static/icons/maskable-512.png
