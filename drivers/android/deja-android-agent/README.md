# Déjà Android Agent (`dev.deja.agent`)

Minimal, zero-Gradle on-device UiAutomator2 HTTP instrumentation agent used by `deja-driver-android`.

## Overview

- **Self-instrumenting package:** `dev.deja.agent` (`AndroidManifest.xml`, `minSdkVersion 24`, `targetSdkVersion 36`).
- **Loopback-only HTTP server:** Binds strictly to `127.0.0.1:<port>` (default `7007`), bridged to the host through `adb -s <serial> forward tcp:0 tcp:<port>`.
- **Constant-time authentication:** Every route except `GET /health` requires `X-Deja-Token` checked via `MessageDigest.isEqual`. `GET /health` exposes only the 16-hex SHA-256 prefix (`tokenDigest`) for warm-session verification.
- **Single-flight busy lock:** Concurrent UI requests fail fast with `409 Conflict` via `ReentrantLock`.
- **Rust-parity FNV-1a hierarchy hashing:** `HierarchySnapshot` computes the exact 64-bit FNV-1a digest matching `deja_flow::backoff::hash_hierarchy_nodes`, pruning volatile SystemUI chrome subtrees and normalizing retained parent ordinals.
- **Zero-fling scroll (`DEJA-85`):** `UiActions.swipe` emits 12 interpolated `ACTION_MOVE` steps, holds at `(endX, endY)` for `holdDurationMs`, emits a final zero-delta `ACTION_MOVE` to decay Android's `VelocityTracker` to zero, and then lifts with `ACTION_UP`.

## Building Locally

Déjà ships a SHA-256-pinned prebuilt agent APK embedded directly into the Rust binary (`prebuilt/deja-android-agent.apk`) for zero-JDK runtime execution. At runtime, Déjà uses:

1. `DEJA_ANDROID_AGENT_APK` (optionally pinned with `DEJA_ANDROID_AGENT_APK_SHA256`), if explicitly set.
2. On-demand compilation from source via `build-apk.sh`, only when `DEJA_ANDROID_BUILD_AGENT_FROM_SOURCE=1` is set (persisting the debug keystore in `$DEJA_HOME/drivers/android/debug.keystore`).
3. Otherwise, the embedded prebuilt APK materialized into `$DEJA_HOME/drivers/android/<sha256>/deja-android-agent.apk`.

To rebuild the prebuilt APK locally:

```bash
drivers/android/deja-android-agent/build-apk.sh \
  --out drivers/android/deja-android-agent/prebuilt/deja-android-agent.apk
```

Never commit arbitrary fixture `.apk`, `.class`, `.dex`, or `.keystore` files to this repository; only `drivers/android/deja-android-agent/prebuilt/deja-android-agent.apk` (`<= 256 KiB`) is allowlisted by `deja-repo-check`.
