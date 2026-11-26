# DEJA-60 Spike Evidence: `DejaXCTestRunner` Native iOS Simulator Proof

This document records the live iOS Simulator spike measurements proving Déjà can drive a real
SwiftUI app (`DejaFixture`) natively from Rust (`deja-driver-ios`) using only Apple's public
`XCTest`, `Foundation`, and `Network` frameworks plus `xcrun simctl` — with **zero Maestro**,
**zero Java**, **zero third-party Swift dependencies**, and **zero private Apple SPI**.

## Environment

- **Host OS**: macOS (Apple Silicon `arm64`)
- **Xcode**: `27.2` (`Build version 27B52`)
- **iOS Simulator SDK**: `27.2`
- **Dedicated Simulator**: `iPhone 17 (deja)` (`iOS 27.2`)
- **Subprocess `PATH` during spike**: `/usr/bin:/bin:/usr/sbin:/sbin` (`which maestro` exits non-zero; no Maestro or JDK directories present)
- **Runner cache key**: `27B52-27.2-arm64-1c26faacc508d014c170ac3f1526160fe0c3287981dca92954b604e2cc57f79b`

## Measured Timings vs. Maestro 2.12 Baseline

| Metric | Native `DejaXCTestRunner` (DEJA-60) | Maestro 2.12 Baseline (`WALK-011`) | Notes |
| --- | ---: | ---: | --- |
| Cold runner build (`xcodebuild build-for-testing`, `arm64`) | `14,767 ms` – `40,625 ms` | N/A (external Java/Maestro install) | Cached once per `<xcode-build>-<ios-sdk>-<arch>-<source-sha256>` under `$DEJA_HOME/ios-runner/builds/` (varies with host CPU load) |
| Cold runner attach (`xcodebuild test-without-building` -> `/health` 200) | `9,813 ms` – `13,625 ms` | — | First attach after cold build |
| Warm runner attach (cache hit, `build_duration_ms = 0`) | `8,243 ms` – `9,231 ms` | — | Reuses cached products via per-run `<udid>.xctestrun` |
| Full `DejaFixture` checkout flow (attached session) | `9,642 ms` – `9,997 ms` | — | `clear_state` + launch (`-DEJA_FIXTURE_ORDER WC-2026`) + login + catalog + cart + confirmation `#WC-2026` |
| **Warm attach + full checkout flow (end-to-end)** | **`17,885 ms` – `19,228 ms`** | **`29,067 ms`** (`remember`) / **`31,258 ms`** (`warm`) | Baseline from [walk011-remember.txt](../../../../spikes/003-ios-deja/evidence/walk011-remember.txt) and [walk011-warm.txt](../../../../spikes/003-ios-deja/evidence/walk011-warm.txt) |
| `/hierarchy` latency (`n = 16` samples, latest run) | `min = 11.7 ms`, `median = 13.9 ms`, `p95 = 3,579.9 ms`, `max = 3,579.9 ms` | — | Steady-state snapshots run in ~12–14 ms; single ~3.6 s max occurs on the very first accessibility snapshot immediately after `simctl launch` |
| PNG screenshot capture (`simctl io screenshot --type=png`) | `116,825 bytes` | — | Valid 8-byte PNG header verified |
| `SIGTERM` mid-run cleanup + process exit | `1,012 ms` – `1,247 ms` | — | Live `xcodebuild test-without-building` process present before `SIGTERM`; `0` orphans after exit |
| Orphaned `xcodebuild test-without-building` PIDs after shutdown | `0` | — | Verified both after normal shutdown and after real `SIGTERM` |

## Verified Capabilities & Findings

1. **All 7 loopback HTTP endpoints verified live on iOS 27.2 Simulator**:
   - `GET /health` (200 OK with valid `X-Deja-Agent-Token`; 401 Unauthorized verified with invalid token)
   - `GET /appState?appId=...` (verified `running_foreground` code `4` during checkout and background transition after `home` key)
   - `GET /hierarchy?appId=...&includeSystemUi=...` (verified foreground app-only snapshots without SpringBoard overhead, finite sanitized bounds, computed `hittable` without `XCUIElementSnapshot.isHittable`, and automatic SpringBoard inclusion when the target app is not foreground)
   - `POST /tap` (screen-point coordinate taps across `username_input`, `password_input`, `login_button`, `add_to_cart`, `checkout_button`)
   - `POST /type` (pre-type focus settle + DEJA-58 keyboard height stability settle across `qa@example.com` and `testing123`)
   - `POST /swipe` (`press(forDuration:thenDragTo:withVelocity:thenHoldForDuration:)` drag gesture)
   - `POST /key` (`"home"` mapped to `XCUIDevice.shared.press(.home)`)
2. **Launch argument & state isolation**:
   - Path-confined `clear_state` wiped app data children while preserving `.com.apple.mobile_container_manager.metadata.plist`.
   - `simctl launch` forwarded `-DEJA_FIXTURE_ORDER WC-2026`, producing `Order #WC-2026 · Mochi sticker pack` in [DejaFixtureApp.swift](../../../../spikes/003-ios-deja/DejaFixture/DejaFixtureApp.swift).
3. **iOS 27.2 Simulator `NWListener` finding**:
   - Setting `NWParameters.acceptLocalOnly = true` on iOS 27.2 Simulator triggers `nw_listener_socket_inbox_create_socket setsockopt SO_NECP_LISTENUUID failed [2: No such file or directory]` and causes the simulator network stack to immediately reset incoming TCP connections (`ECONNRESET`).
   - Binding strictly via `params.requiredLocalEndpoint = .hostPort(host: .ipv4(.loopback), port: ...)` (verified via `lsof` as `127.0.0.1:<ephemeral> (LISTEN)`) paired with an explicit loopback peer check in `accept(_:)` plus constant-time `X-Deja-Agent-Token` authentication works reliably on iOS 27.2 Simulator.
4. **Xcode `.xctestrun` runner bundle ID finding**:
   - Xcode 27.2 hosts UI test runners under `<PRODUCT_BUNDLE_IDENTIFIER>.xctrunner` (`dev.deja.ios.xctest-runner.xctrunner`), so shutdown terminates `dev.deja.ios.xctest-runner.xctrunner`, `dev.deja.ios.xctest-runner`, and `dev.deja.ios.runner-host` in addition to signaling the isolated `xcodebuild` process group and running the UDID-scoped `ps` sweep (`DEJA-54`).

## Reproducing the Spike

```sh
cd rust
DEJA_IOS_TEST_UDID=<dedicated-simulator-udid> \
  cargo test --locked -p deja-driver-ios --test ios_simulator_spike -- --ignored --nocapture --test-threads=1
```

## Source Artifacts

- Runner host stub: [DejaRunnerHostApp.swift](DejaRunnerHost/DejaRunnerHostApp.swift)
- XCTest agent harness: [DejaXCTestRunnerAgent.swift](DejaXCTestRunner/DejaXCTestRunnerAgent.swift)
- Loopback HTTP/1.1 server: [HttpServer.swift](DejaXCTestRunner/HttpServer.swift)
- Public XCUI action handlers: [XcuiActions.swift](DejaXCTestRunner/XcuiActions.swift)
- Live simulator spike suite: [ios_simulator_spike.rs](../../../crates/deja-driver-ios/tests/ios_simulator_spike.rs)
