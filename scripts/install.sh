#!/bin/sh
set -eu
if [ "$#" -ne 1 ]; then
  echo "Usage: $0 <Jimmy release .tar.gz URL>" >&2
  exit 2
fi
url=$1
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT HUP INT TERM
curl -fSL "$url" -o "$work/jimmy.tar.gz"
curl -fSL "$url.sha256" -o "$work/checksum"
expected=$(awk 'NR == 1 { print tolower($1) }' "$work/checksum")
if command -v sha256sum >/dev/null; then
  actual=$(sha256sum "$work/jimmy.tar.gz" | awk '{print $1}')
else
  actual=$(shasum -a 256 "$work/jimmy.tar.gz" | awk '{print $1}')
fi
[ -n "$expected" ] && [ "$expected" = "$actual" ] || { echo "Jimmy archive checksum mismatch" >&2; exit 1; }
tar -xOzf "$work/jimmy.tar.gz" jimmy > "$work/jimmy"
bindir=${JIMMY_INSTALL_DIR:-"$HOME/.local/bin"}
mkdir -p "$bindir"
install -m 0755 "$work/jimmy" "$bindir/jimmy"
echo "$bindir/jimmy"
