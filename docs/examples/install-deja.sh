#!/usr/bin/env bash
# Download a pinned deja release binary, verify its sha256 and print its path.
# Pin both values in your repo. Nothing here trusts "latest".
#
#   DEJA_VERSION=0.1.0 DEJA_SHA256=<64 hex chars> docs/examples/install-deja.sh
#   deja="$(DEJA_VERSION=0.1.0 DEJA_SHA256=... docs/examples/install-deja.sh)"
#   "$deja" run "checkout" --config deja.config.json --summary
set -euo pipefail

: "${DEJA_VERSION:?Set DEJA_VERSION, for example 0.1.0}"
: "${DEJA_SHA256:?Set DEJA_SHA256 to the checksum for this version and platform}"
repo="${DEJA_REPO:-seankim-android/deja}"
cache="${DEJA_BIN_CACHE:-$HOME/.cache/deja-bin}"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) platform="darwin-arm64" ;;
  Linux-x86_64) platform="linux-x64" ;;
  *) echo "No deja binary for $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

name="deja-${DEJA_VERSION}-${platform}"
binary="$cache/$name"
sha_of() { shasum -a 256 "$1" | awk '{print $1}'; }

if [ -x "$binary" ] && [ "$(sha_of "$binary")" = "$DEJA_SHA256" ]; then
  echo "$binary"
  exit 0
fi

mkdir -p "$cache"
partial="$(mktemp "$cache/$name.XXXXXX")"
trap 'rm -f "$partial"' EXIT
curl --fail --silent --show-error --location \
  --output "$partial" \
  "https://github.com/$repo/releases/download/v${DEJA_VERSION}/$name"

actual="$(sha_of "$partial")"
if [ "$actual" != "$DEJA_SHA256" ]; then
  echo "Checksum mismatch for $name: expected $DEJA_SHA256, got $actual" >&2
  exit 1
fi
chmod +x "$partial"
mv "$partial" "$binary"
trap - EXIT
echo "$binary"
