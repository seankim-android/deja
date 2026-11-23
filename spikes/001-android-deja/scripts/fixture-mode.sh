#!/bin/sh
# Toggle only the demo Checkout resource ID on the configured device. No app/cache files are edited.
set -eu

device="${DEJA_DEVICE:-emulator-5554}"
selector_setting="walkcache_checkout_selector_stale"
regression_setting="walkcache_product_regression"

case "${1:-}" in
  stable)
    adb -s "$device" shell settings delete global "$selector_setting" >/dev/null
    adb -s "$device" shell settings delete global "$regression_setting" >/dev/null
    ;;
  stale)
    adb -s "$device" shell settings put global "$selector_setting" 1 >/dev/null
    adb -s "$device" shell settings delete global "$regression_setting" >/dev/null
    ;;
  product-regression)
    adb -s "$device" shell settings delete global "$selector_setting" >/dev/null
    adb -s "$device" shell settings put global "$regression_setting" 1 >/dev/null
    ;;
  *)
    echo "Usage: sh scripts/fixture-mode.sh stable|stale|product-regression" >&2
    exit 2
    ;;
esac

printf 'Checkout fixture mode on %s: %s\n' "$device" "$1"
