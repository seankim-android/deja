<h1 align="center"><picture><source media="(prefers-color-scheme: dark)" srcset="docs/brand/deja-logo-dark.svg"><img alt="Déjà" src="docs/brand/deja-logo.svg" width="280"></picture></h1>

<p align="center">
Verified app test memory and native <code>maestro/v1</code> runner in Rust for iOS, Android, and Web. Learn or bring a flow once, verify it cleanly, then replay it with zero model calls.
</p>

<p align="center">
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/badge/License-Apache%202.0-blue.svg"/></a>
  <a href="https://github.com/deja-run/deja/actions/workflows/rust-checks.yml"><img alt="Rust checks" src="https://github.com/deja-run/deja/actions/workflows/rust-checks.yml/badge.svg"/></a>
</p>

<p align="center"><img src="demo/deja-preview.gif" alt="Recorded prototype walkthrough showing cold exploration, zero-model warm replay, scoped selector repair, and product regression failure" width="80%"/></p>

<p align="center"><sub>Recorded on the retired prototype (1.5x speed, silent): cold exploration, zero-model replay, scoped selector repair, and product regression failure.</sub></p>

## How Déjà works

AI testing agents can navigate an app on their own, yet most call the model again on every run just to repeat flows that already passed.

Déjà splits learning a test from running it. You start with a flow from cold exploration or `maestro/v1` YAML you already keep in Git, and Déjà executes it end-to-end before saving it or updating `deja.walks.json`. Once verified for a build, replays run with zero model calls and no API key. Selector drift can produce a scoped repair proposal, while a failing product assertion stays failed so real regressions never pass. Flows stay plain [Maestro](https://github.com/mobile-dev-inc/Maestro)-compatible YAML that teams review in pull requests.

## What works today

Déjà is mid-rewrite into a standalone pure-Rust binary with no JVM, Node, Bun, or external Maestro dependency. Right now on `main`, the Cargo workspace, native iOS Simulator runner spike, flow primitives, embedded agent guide, and repository checks compile and run. Full `maestro/v1` YAML execution, SQLite v5 persistence, CLI replay/gating, Android, Web, and Graph Explorer are still in progress.

| Component | Status on `main` | Reference |
| --- | --- | --- |
| Cargo workspace and DAG | 9 crates (`edition = "2024"`, MSRV `1.89`, `unsafe_code = "deny"`, `rustls` only), boundary rules enforced in tests | [Cargo.toml](Cargo.toml), [workspace_boundaries.rs](crates/deja-cli/tests/workspace_boundaries.rs) |
| Native iOS Simulator runner spike | `deja-driver-ios` + embedded Swift `DejaXCTestRunner` verified live on iOS Simulator across 7 loopback endpoints, runner build caching, and zero-orphan `SIGTERM` teardown (0 Maestro/JVM) | [SPIKE_EVIDENCE.md](drivers/ios/DejaXCTestRunner/SPIKE_EVIDENCE.md) |
| Flow primitives | `PollSchedule`, `SettleTracker`, chrome-stripped `hash_hierarchy_nodes`, `DriverCapabilities`, `ObservationSource`, `VisibleContext`, and `RunTimingBreakdown` | [docs/PERFORMANCE.md](docs/PERFORMANCE.md) |
| Embedded agent guide | Printable directly via `deja guide` | [docs/AGENT_GUIDE.md](docs/AGENT_GUIDE.md) |
| Repository validator | `deja-repo-check` verifies tracked artifacts, local Markdown links, JSON syntax, leaks, clean working tree, and `docs/results-chart.svg` parity | [docs/results-chart.svg](docs/results-chart.svg) |

## Quick start

You need Rust `1.89` or newer. Running the live iOS Simulator spike also requires macOS, Xcode, and an iOS Simulator runtime (no Bun, Node, or Java required).

Clone the repo and run the workspace tests, the embedded agent guide, and the repository validator:

```sh
git clone https://github.com/deja-run/deja.git
cd deja

cargo test --workspace
cargo run -- guide
cargo run --bin deja-repo-check -- --require-clean --check-chart
```

To run the live iOS Simulator spike (`DEJA-60`), boot a dedicated simulator and pass its UDID via `DEJA_IOS_TEST_UDID`. Never point this at a simulator used by another task or run `simctl shutdown all`.

```sh
UDID=$(xcrun simctl create deja com.apple.CoreSimulator.SimDeviceType.iPhone-17)
xcrun simctl boot "$UDID" && xcrun simctl bootstatus "$UDID" -b

DEJA_IOS_TEST_UDID="$UDID" \
  cargo test --locked -p deja-driver-ios --test ios_simulator_spike -- --ignored --nocapture --test-threads=1
```

Delete the temporary simulator when finished:

```sh
xcrun simctl delete "$UDID"
```

## Workspace layout

The workspace enforces a strict dependency DAG in [workspace_boundaries.rs](crates/deja-cli/tests/workspace_boundaries.rs). `deja-core` keeps a closed-world dependency allowlist with no SQLite, HTTP, async runtime, or driver crates; concrete drivers cannot depend on one another; and `deja-cli` is the only composition root.

| Crate | Responsibility | Current state |
| --- | --- | --- |
| `deja-core` | Domain contracts, YAML 1.2 closure, assertion hashing, manifest validation, redaction, streaming build fingerprinting | Scaffolded (`DEJA-61`) |
| `deja-store` | Unified SQLite v5 store (`rusqlite` `bundled`), stat cache, writer locks, manifest persistence | Scaffolded (`DEJA-62`) |
| `deja-flow` | Platform-neutral `maestro/v1` interpreter, polling/settle, capability contracts, failure classification, telemetry, JUnit XML | Polling, settle, capabilities, and timing primitives landed; interpreter in `DEJA-63` |
| `deja-driver-ios` | Native iOS Simulator driver (`xcrun simctl` + embedded `DejaXCTestRunner` Swift companion) | Live spike proven (`DEJA-60`); production integration in `DEJA-64` |
| `deja-driver-android` | Native Android driver (`adb` + bundled UiAutomator2 APK, zero host JDK) | Scaffolded (`DEJA-68`) |
| `deja-driver-web` | Direct Chromium Chrome DevTools Protocol driver (`tokio-tungstenite`, zero Node/Playwright) | Scaffolded (`DEJA-69`) |
| `deja-explore` | Graph Explorer, action choosers, path memory, bounded step repair | Scaffolded (`DEJA-71`..`DEJA-72`) |
| `deja-mcp` | Local stdio JSON-RPC 2.0 MCP server for coding agents | Scaffolded (`DEJA-67`) |
| `deja-cli` | Composition root (`deja` CLI and `deja-repo-check`) | `guide` and `deja-repo-check` runnable; full CLI in `DEJA-65`..`DEJA-66` |

## Live iOS runner results (DEJA-60)

Measured on Apple Silicon (`iPhone 17` simulator, `iOS 27.2`, Xcode `27.2` `27B52`) with `PATH=/usr/bin:/bin:/usr/sbin:/sbin` (no Maestro or JDK on `PATH`). The spike builds `DejaFixture`, clears its container, launches with `-DEJA_FIXTURE_ORDER WC-2026`, completes login, cart, and `#WC-2026` confirmation, captures a PNG screenshot, exercises `/swipe` and `/key`, and verifies `SIGTERM` cleanup.

| Metric | Native `DejaXCTestRunner` (DEJA-60) | Maestro 2.12 (WALK-011) | Notes |
| --- | ---: | ---: | --- |
| Cold runner build (`xcodebuild build-for-testing`) | `14.8–40.6 s` | n/a | Cached per `<xcode>-<sdk>-<arch>-<sha256>` (`34,633 ms` in latest run) |
| Warm runner attach (`test-without-building` -> `/health`) | `8.2–9.4 s` | - | Reuses cached runner build (`9,399 ms` in latest run) |
| Attached `DejaFixture` checkout flow | `9.6–10.3 s` | - | Full flow inside attached session (`10,298 ms` in latest run) |
| Steady-state `/hierarchy` snapshot | `11.1–19.5 ms` median | - | `3.6–3.9 s` p95 spike on first post-launch accessibility read (`n = 16`) |
| **Warm attach + full checkout E2E** | **`17.9–19.7 s`** | **`29.1–31.3 s`** | `19,697 ms` vs. `29,067 ms` (`remember`) / `31,258 ms` (`warm`); ~35–40% faster, 0 Maestro/JVM |
| `SIGTERM` mid-run cleanup | `2,418 ms` | - | `0` orphaned `xcodebuild` PIDs after normal exit or `SIGTERM` |

Full timings, endpoints, and iOS 27.2 `NWListener` findings are documented in [SPIKE_EVIDENCE.md](drivers/ios/DejaXCTestRunner/SPIKE_EVIDENCE.md) and [docs/PERFORMANCE.md](docs/PERFORMANCE.md).

## Prototype baseline evidence

Recorded run reports in `demo/live-evidence/` from the retired TypeScript/Bun + Maestro 2.12 prototype (`Kimi K3.1 Pro` on `Cerebras` for cold exploration) established model avoidance (`0` model calls on replay and bounded repair) and showed that >99% of wall-clock time sat in JVM/Maestro startup, driver attach, and polling sleeps rather than Déjà orchestration.

<p align="center"><img src="docs/results-chart.svg" alt="Bar charts of model calls and duration per run: Android cold explore 13 calls, 97.9 s; Android warm replay 0 calls, 26.1 s; Android stale selector repair 0 calls, 66.9 s; Chromium warm replay 0 calls, 36.6 s" width="100%"/></p>

| Run | Evidence file | Outcome | Model calls | Assertions | Duration |
| --- | --- | --- | ---: | ---: | ---: |
| Android cold explore | `android/cold-run-report.json` | cold-recorded | 13 | 14 | 97.9 s |
| Android warm replay | `android/warm-run-report.json` | passed (cache hit) | 0 | 14 | 26.1 s |
| Android stale selector repair | `android/stale-repair-run-report.json` | repaired (1 repair) | 0 | 12 | 66.9 s (repair step) |
| Chromium warm replay | `chromium/run-run-report.json` | passed (cache hit) | 0 | 12 | 36.6 s |

## Roadmap

Active engineering on `main` follows the Linear backlog in platform order (`iOS -> Android -> Web`):

- `DEJA-61` (`deja-core`): Pure-Rust contracts, `deja.walks.json` v1 hash parity, YAML 1.2 flow closures, redaction, and streaming build fingerprinting.
- `DEJA-62` (`deja-store`): Unified SQLite v5 store, `build_fingerprint_cache`, writer locks, and consumer manifest persistence.
- `DEJA-63` (`deja-flow`): Full `maestro/v1` YAML interpreter, selector matching, failure classification, JS scripting, and JUnit XML.
- `DEJA-64` (`deja-driver-ios`): Production `PlatformDriver` implementation, compound `POST /act`, server-side `POST /waitForSelector`, `.ips` crash attribution, and animation disabling.
- `DEJA-65`..`DEJA-67` (`deja-cli`, `deja-mcp`): Full native CLI (`doctor`, `manifest check`, `remember`, `run`, `gate`, `list`, `inspect`), dynamic `--devices` sharding, `deja-summary v=1`, stdio JSON-RPC 2.0 `deja mcp` with cancellation, and standalone release binaries.
- `DEJA-68` (`deja-driver-android`): Native `adb` + bundled UiAutomator2 instrumentation APK with zero host JDK at runtime.
- `DEJA-69` (`deja-driver-web`): Direct Chromium CDP driver over WebSocket with zero Node or Playwright.
- `DEJA-71`..`DEJA-72` (`deja-explore`): Cross-platform Graph Explorer, path memory, subflow reuse, and bounded selector repair proposals.

## Docs

- [docs/PERFORMANCE.md](docs/PERFORMANCE.md): Latency baselines, Amdahl bottlenecks, and optimization roadmap.
- [docs/AGENT_GUIDE.md](docs/AGENT_GUIDE.md): Embedded agent guide printed by `deja guide`.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and [docs/REFERENCE.md](docs/REFERENCE.md): `deja.walks.json` v1, closure/assertion-hashing rules, failure classification, and `maestro/v1` behavior contracts (some sections still reflect pre-cutover TypeScript internals while `DEJA-61` through `DEJA-67` land).
- [docs/RELEASING.md](docs/RELEASING.md): Release binary targets and verification workflow.
- Platform fixture and spike records: [iOS](spikes/003-ios-deja/README.md), [Android](spikes/001-android-deja/README.md), and [Web](spikes/002-web-deja/README.md).

## History

Déjà began as a hackathon prototype validating zero-model replay with Maestro and TypeScript. That prototype was retired so Déjà could be rebuilt from the ground up as a standalone native Rust runner and test memory layer.

## Contributing

Pull requests from outside contributors are not accepted right now. Bug reports and ideas are welcome as [GitHub issues](https://github.com/deja-run/deja/issues). See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[Apache-2.0](LICENSE)

The Déjà name is covered by the draft [trademark policy](TRADEMARKS.md).
