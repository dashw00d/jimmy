#!/bin/sh
set -eu
if [ "$#" -gt 1 ]; then
  echo "Usage: $0 [Jimmy release .tar.gz URL]" >&2
  exit 2
fi
if [ "$#" -eq 1 ]; then
  url=$1
else
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
    Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu ;;
    Darwin-arm64) target=aarch64-apple-darwin ;;
    Darwin-x86_64) target=x86_64-apple-darwin ;;
    *) echo "Jimmy has no build for $(uname -s) $(uname -m) yet" >&2; exit 1 ;;
  esac
  url="https://github.com/dashw00d/jimmy/releases/latest/download/jimmy-$target.tar.gz"
fi
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
