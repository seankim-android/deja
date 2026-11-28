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
- **Runner cache key**: `27B52-27.2-arm64-eba457d4fa8f5a3c900abe4ff71a06c3755975464ae2eee05563900ef2b2b210`

## Measured Timings vs. Maestro 2.12 Baseline

| Metric | Native `DejaXCTestRunner` (DEJA-60) | Maestro 2.12 Baseline (`WALK-011`) | Notes |
| --- | ---: | ---: | --- |
| Cold runner build (`xcodebuild build-for-testing`, `arm64`) | `34,633 ms` | N/A (external Java/Maestro install) | Cached once per `<xcode-build>-<ios-sdk>-<arch>-<source-sha256>` under `$DEJA_HOME/ios-runner/builds/` (observed `14.8–40.6 s` across runs depending on host load) |
| Cold runner attach (`xcodebuild test-without-building` -> `/health` 200) | `19,126 ms` | — | First attach after cold build (observed `9.8–19.1 s`) |
| Warm runner attach (cache hit, `build_duration_ms = 0`) | `9,399 ms` | — | Reuses cached products via per-run `<udid>.xctestrun` (observed `8.2–9.4 s`) |
| Full `DejaFixture` checkout flow (attached session) | `10,298 ms` | — | `clear_state` + launch (`-DEJA_FIXTURE_ORDER WC-2026`) + login + catalog + cart + confirmation `#WC-2026` (observed `9.6–10.3 s`) |
| **Warm attach + full checkout flow (end-to-end)** | **`19,697 ms`** | **`29,067 ms`** (`remember`) / **`31,258 ms`** (`warm`) | Baseline from [walk011-remember.txt](../../../spikes/003-ios-deja/evidence/walk011-remember.txt) and [walk011-warm.txt](../../../spikes/003-ios-deja/evidence/walk011-warm.txt) |
| `/hierarchy` latency (`n = 16` samples, latest run) | `min = 11.1 ms`, `median = 19.5 ms`, `p95 = 3,872.2 ms`, `max = 3,872.2 ms` | — | Steady-state snapshots run in ~11–20 ms; single ~3.9 s max occurs on the very first accessibility snapshot immediately after `simctl launch` |
| PNG screenshot capture (`simctl io screenshot --type=png`) | `116,244 bytes` | — | Valid 8-byte PNG header verified |
| `SIGTERM` mid-run cleanup + process exit | `2,418 ms` | — | Live `xcodebuild test-without-building` process present before `SIGTERM`; `0` orphans after exit |
| Orphaned `xcodebuild test-without-building` PIDs after shutdown | `0` | — | Verified both after normal shutdown and after real `SIGTERM` |

## Verified Capabilities & Findings

1. **All 10 loopback HTTP endpoints verified live on iOS 27.2 Simulator**:
   - `GET /health` (200 OK with valid `X-Deja-Agent-Token`; 401 Unauthorized verified with invalid token)
   - `GET /appState?appId=...` (verified `running_foreground` code `4` during checkout and background transition after `home` key)
   - `GET /hierarchy?appId=...&includeSystemUi=...&knownHash=...` (verified foreground app-only snapshots without SpringBoard overhead, finite sanitized bounds, computed `hittable` without `XCUIElementSnapshot.isHittable`, chrome-stripped 64-bit FNV-1a `hierarchyHash` matching `deja_flow::backoff::hash_hierarchy_nodes` bit-for-bit, `knownHash` short-circuit returning `unchanged: true` with `nodes: []`, and automatic SpringBoard inclusion without stalling on backgrounded/suspended apps)
   - `POST /tap` (screen-point coordinate taps across `username_input`, `password_input`, `login_button`, `add_to_cart`, `checkout_button`)
   - `POST /type` (single-snapshot pre-type focus settle with out-of-process keyboard fallback + DEJA-58 keyboard height stability settle across `qa@example.com` and `testing123`)
   - `POST /swipe` (`press(forDuration:thenDragTo:withVelocity:thenHoldForDuration:)` drag gesture, including `holdDurationMs` zero-fling drag-and-hold)
   - `POST /key` (`"home"` mapped to `XCUIDevice.shared.press(.home)`)
   - `POST /act` (compound action + optional `postActionDelayMs` + optional `returnHierarchy` / `knownHash` in one HTTP round-trip)
   - `POST /waitForSelector` (server-side `15..150 ms` adaptive backoff wait for `identifier`, `text`, `textMatch`, `notVisible`, and `waitStable`, returning pruned `visibleSample` `<= 30` items on timeout with zero extra round-trips)
   - `POST /warmup` (post-launch `running_foreground` wait + initial AX snapshot warmup with optional `returnHierarchy`)
2. **Launch argument & state isolation**:
   - Path-confined `clear_state` wiped app data children while preserving `.com.apple.mobile_container_manager.metadata.plist`.
   - `simctl launch` forwarded `-DEJA_FIXTURE_ORDER WC-2026` (and `WC-2027` on warm session reuse), producing `Order #WC-2026 · Mochi sticker pack` and `Order #WC-2027 · Mochi sticker pack` in [DejaFixtureApp.swift](../../../spikes/003-ios-deja/DejaFixture/DejaFixtureApp.swift).
3. **iOS 27.2 Simulator `NWListener` finding**:
   - Setting `NWParameters.acceptLocalOnly = true` on iOS 27.2 Simulator triggers `nw_listener_socket_inbox_create_socket setsockopt SO_NECP_LISTENUUID failed [2: No such file or directory]` and causes the simulator network stack to immediately reset incoming TCP connections (`ECONNRESET`).
   - Binding strictly via `params.requiredLocalEndpoint = .hostPort(host: .ipv4(.loopback), port: ...)` (verified via `lsof` as `127.0.0.1:<ephemeral> (LISTEN)`) paired with an explicit loopback peer check in `accept(_:)` plus constant-time `X-Deja-Agent-Token` authentication works reliably on iOS 27.2 Simulator.
4. **Xcode `.xctestrun` runner bundle ID finding**:
   - Xcode 27.2 hosts UI test runners under `<PRODUCT_BUNDLE_IDENTIFIER>.xctrunner` (`dev.deja.ios.xctest-runner.xctrunner`), so shutdown terminates `dev.deja.ios.xctest-runner.xctrunner`, `dev.deja.ios.xctest-runner`, and `dev.deja.ios.runner-host` in addition to signaling the isolated `xcodebuild` process group and running the UDID-scoped `ps` sweep (`DEJA-54`).
5. **Backgrounded-app AX snapshot hang finding (`DEJA-77`)**:
   - Calling `app.snapshot()` while `app.state` is `.runningBackground` or `.runningBackgroundSuspended` (immediately after pressing Home) can stall XCTest's AX IPC for >30s on iOS 27.2 Simulator. Restricting `app.snapshot()` to `state == .runningForeground` and querying SpringBoard directly whenever `state != .runningForeground` eliminates the hang cleanly.

## DEJA-77 Live Benchmark: Compound Endpoints & Warm Session Reuse

Measured on a fresh dedicated temporary `iPhone 17` (`iOS 27.2`, Xcode `27.2` `27B52`) via `benchmark_deja_fixture_optimized_and_session_reuse` (runner cache key `27B52-27.2-arm64-9a7966c6d4f1543ab2f75d3bab2fd24f7bc48a33884f448ed76c0d6fc2a879c0`):

| Metric | Legacy per-endpoint flow (`DEJA-60` style) | Optimized `DEJA-77` flow | Delta / Notes |
| --- | ---: | ---: | --- |
| Warm runner attach (`test-without-building` → `/health`) | `7,421 ms` | `7,421 ms` (Flow #1) / **`0 ms`** (Flow #2 reuse) | `verify_healthy()` took `0.62–0.70 ms`; eliminates `~7.4–10.7 s` attach on every reused flow |
| Single `DejaFixture` checkout flow (attached) | `10,220 ms` (`14` HTTP calls) | **`8,956 ms`** (`8` HTTP calls) | **-12.4% wall-clock**, **-42.9% HTTP round-trips** (observed `8.96–9.09 s` vs `10.22–14.14 s` across runs) |
| Flow #2 on same warm session (`WC-2027`) | `17,641 ms` (if re-attaching) | **`9,079 ms`** (`8` HTTP calls, `0 ms` attach) | Verified same runner PID + clean `#WC-2027` state isolation |
| **2-flow suite total (warm attach + 2 checkouts)** | **`35,282 ms`** (2× attach + 2× baseline) | **`25,456 ms`** (1× attach + 2× optimized) | **-27.9% end-to-end on 2 flows**; Warm attach + Flow #1 (`16,377 ms`) is **1.77×–1.91× faster** than Maestro 2.12 (`29.1–31.3 s`) |
| Steady-state `/hierarchy` (`n = 8` pairs) | `14.83 ms` median (`16.53 ms` p95) | `14.97 ms` median (`17.93 ms` p95) with `knownHash` | `knownHash` verified `unchanged: true`, `nodes: []`, exact Rust↔Swift FNV-1a parity; tiny 30-node fixture is AX-IPC-bound (~15 ms), while large trees skip JSON encoding & transfer |
| `/waitForSelector` timeout diagnostics | Extra `/hierarchy` call required | **`9` pruned `visibleSample` items** inline | Returned inside the timeout response (`<= 30` items, zero extra round-trips) |
| `SIGTERM` cleanup / orphaned `xcodebuild` PIDs | `1,367 ms` / `0` orphans | `0` orphans | Verified after normal shutdown and real `SIGTERM` |

## Reproducing the Spike

```sh
DEJA_IOS_TEST_UDID=<dedicated-simulator-udid> \
  cargo test --locked -p deja-driver-ios --test ios_simulator_spike -- --ignored --nocapture --test-threads=1
```

## Source Artifacts

- Runner host stub: [DejaRunnerHostApp.swift](DejaRunnerHost/DejaRunnerHostApp.swift)
- XCTest agent harness: [DejaXCTestRunnerAgent.swift](DejaXCTestRunner/DejaXCTestRunnerAgent.swift)
- Loopback HTTP/1.1 server: [HttpServer.swift](DejaXCTestRunner/HttpServer.swift)
- Public XCUI action handlers: [XcuiActions.swift](DejaXCTestRunner/XcuiActions.swift)
- Live simulator spike suite: [ios_simulator_spike.rs](../../../crates/deja-driver-ios/tests/ios_simulator_spike.rs)
