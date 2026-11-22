#!/bin/sh
# Toggle only the demo Checkout resource ID on the configured device. No app/cache files are edited.
set -eu

device="${WALKCACHE_DEVICE:-emulator-5554}"
setting="walkcache_checkout_selector_stale"

case "${1:-}" in
  stable)
    adb -s "$device" shell settings delete global "$setting" >/dev/null
    ;;
  stale)
    adb -s "$device" shell settings put global "$setting" 1 >/dev/null
    ;;
  *)
    echo "Usage: sh scripts/fixture-mode.sh stable|stale" >&2
    exit 2
    ;;
esac

printf 'Checkout fixture mode on %s: %s\n' "$device" "$1"
