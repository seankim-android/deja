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

Déjà automatically builds and caches the agent APK on demand into `$DEJA_HOME/drivers/android/<source_sha256>/deja-android-agent.apk` when Android SDK build tools are available, or uses a prebuilt APK supplied via `DEJA_ANDROID_AGENT_APK` (optionally pinned with `DEJA_ANDROID_AGENT_APK_SHA256`).

To compile manually into a temporary path:

```bash
chmod +x drivers/android/deja-android-agent/build-apk.sh
drivers/android/deja-android-agent/build-apk.sh --out /tmp/deja-android-agent.apk
```

Never commit built `.apk`, `.class`, `.dex`, or `.keystore` files to this repository.
