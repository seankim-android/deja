# Déjà Performance Architecture

This document tracks **Déjà**'s latency baselines, Amdahl's Law bottlenecks, architectural optimization levers, and crate boundary rules as Déjà completes its migration to a standalone pure-Rust runner across iOS, Android, and Web.

## Measured Baselines

All baseline figures below come directly from recorded run reports and spike evidence in this repository.

### Legacy Bun/TypeScript + Maestro 2.12 (`demo/live-evidence/` & `spikes/`)

| Run | Total duration | Model calls | Source |
| --- | ---: | ---: | --- |
| Android cold explore (OpenClaw + Kimi + MCP + verify) | `97.9 s` | `13` | [cold-run-report.json](../demo/live-evidence/android/cold-run-report.json) |
| Android warm replay | `26.1 s` | `0` | [warm-run-report.json](../demo/live-evidence/android/warm-run-report.json) |
| Android stale selector repair (diagnosis + clean + verify) | `66.9 s` | `0` | [stale-repair-run-report.json](../demo/live-evidence/android/stale-repair-run-report.json) |
| Chromium warm replay | `36.6 s` | `0` | [run-run-report.json](../demo/live-evidence/chromium/run-run-report.json) |
| iOS warm replay (`WALK-011`, Maestro 2.12) | `31.3 s` | `0` | [walk011-warm-report.json](../spikes/003-ios-deja/evidence/walk011-warm-report.json) |

In the legacy architecture, Déjà's own orchestration overhead was ~3–19 ms; >99% of wall-clock time lived in JVM/Maestro startup, driver attach, polling sleeps, and unbatched model/MCP loops.

### Native Rust + `DejaXCTestRunner` iOS Benchmarks (`DEJA-60` & `DEJA-77`)

Measured on Apple Silicon (`iPhone 17`, iOS `27.2`, Xcode `27.2` `27B52`) in [SPIKE_EVIDENCE.md](../drivers/ios/DejaXCTestRunner/SPIKE_EVIDENCE.md):

| Phase | `DEJA-60` baseline | `DEJA-77` optimized | Primary gain / bottleneck |
| --- | ---: | ---: | --- |
| Cold runner build (`xcodebuild build-for-testing`) | `14.8–40.6 s` | `13.6 s` (cached) | One-time Xcode compile per `<xcode>-<sdk>-<arch>-<sha256>` |
| Warm runner attach (`test-without-building` → `/health`) | `8.2–9.4 s` | `7.4 s` (Flow #1) / **`0 ms`** (Flow #2 reuse) | `verify_healthy()` takes `0.6–0.7 ms`; session reuse drops attach to `0 ms` for subsequent flows |
| Attached `DejaFixture` checkout flow | `10.2 s` (`14` HTTP calls) | **`8.96 s`** (`8` HTTP calls) | Compound `/act`, `/warmup`, server-side `/waitForSelector`, single-snapshot `/type` (**-42.9% HTTP calls**, **-12.4% flow time**; observed `8.96–9.09 s` vs `10.22–14.14 s`) |
| Steady-state `/hierarchy` snapshot | `11.1–19.5 ms` median | `14.8 ms` full / `15.0 ms` `knownHash` short-circuit | Exact Rust↔Swift chrome-stripped FNV-1a `hierarchyHash`; `unchanged: true` skips JSON node serialization & transfer |
| **Warm attach + Flow #1 E2E** | **`17.9–19.7 s`** | **`16.4 s`** | **1.77×–1.91× faster** than Maestro 2.12 (`29.1–31.3 s`) |
| **2-flow suite total (warm attach + 2 checkouts)** | `35.3 s` (2× attach + 2× baseline) | **`25.5 s`** (1× attach + 2× optimized) | **-27.9% end-to-end on 2 flows**; scales as attach cost amortizes across the suite |

## Optimization Roadmap (Ordered by Wall-Clock Impact)

1. **Eliminate per-invocation attach tax (`DEJA-77`, `DEJA-66`, `DEJA-78`)**
   - Reuse a single warm `PlatformDriver` session across sequential flows in batch/sharded runs (`DEJA-66`, `DEJA-77`) — drops second-flow attach from `8.2–9.4 s` to `0 ms`.
   - Persistent local warm driver daemon (`deja daemon`, `DEJA-78`) over a `0600` Unix socket with idle TTL and per-device lease locking — eliminates the `8.2–9.4 s` attach tax across separate CLI/MCP invocations.
2. **Collapse driver round-trips & polling overhead (`DEJA-76`, `DEJA-77`, `DEJA-63`, `DEJA-64`, `DEJA-68`, `DEJA-69`)**
   - Compound `POST /act` (action + optional `returnHierarchy` / `knownHash`) cuts loopback round-trips in half per step.
   - Server-side `POST /waitForSelector` moves adaptive `15..150 ms` polling inside `DejaXCTestRunner` and `deja-android-agent`, returning a pruned `VisibleContext` (`top 30` visible labels/IDs) on timeout to eliminate follow-up inspection calls during exploration and repair.
   - Chrome-stripped `hierarchyHash` (excluding volatile `StatusBar` / system chrome) enables `unchanged: true` short-circuits, reliable `SettleTracker` end-of-list detection, and settled fail-fast on missing selectors.
   - Hold-before-lift scroll gestures (`holdDurationMs` on iOS, `motionevent` / drag-and-hold on Android) avoid unpredictable momentum fling during `scrollUntilVisible`.
   - Disable animations by default on iOS/Android/Web (`window_animation_scale=0`, `prefers-reduced-motion: reduce`) with opt-out.
3. **Skip redundant setup & unchanged work (`DEJA-79`, `DEJA-80`, `DEJA-62`, `DEJA-81`)**
   - Persistent SQLite stat cache (`build_fingerprint_cache` keyed by `(path, size, mtime_ns, ino)`) avoids re-reading unchanged app bundles (`0` bytes read on cache hit), paired with WAL mode, `prepare_cached`, and indexed O(1) lookups (`DEJA-62`).
   - Verified app-state checkpoints (`--checkpoints`, `DEJA-79`): APFS copy-on-write (`/bin/cp -cR`) on iOS simulators and CDP `storageState` on Web keyed by `(build_fingerprint, runtime, prefix_closure_sha256, app_id)`.
   - Affected-flow closure selection (`--changed-from <git-ref>`, `DEJA-80`) and LPT (Longest-Processing-Time-first) shard scheduling (`DEJA-66`).
   - CI runner warmup caching (`deja runner warmup`, `DEJA-81`) and read-only remote validation CAS seam.
4. **Cut LLM turns & token overhead in cold exploration (`DEJA-71`, `DEJA-72`, `DEJA-67`)**
   - Batched form-fill + submit turns, static KV-cacheable instruction prefixes, pruned interactive observations, and batched `deja_run_steps` MCP execution.
   - Sub-ms indexed path-memory lookup with in-memory current-build action/answer caching (`DEJA-72`).
   - Optional local Vision OCR fallback tagged `source: "pixels"` (strictly separated from deterministic `source: "tree"` accessibility nodes) for Canvas/Metal/game surfaces during exploration and repair.
5. **Continuous measurement (`DEJA-76`, `DEJA-82`)**
   - `deja-flow::timing` provides `RunTimingBreakdown`, `StepTiming`, and `LatencyStats` (`min`/`p50`/`p95`/`max`).
   - `deja bench` (`DEJA-82`) provides a zero-external-dependency microbenchmark and live E2E regression harness.

## Crate Boundary & Dependency Rules

Every optimization respects [`workspace_boundaries.rs`](../crates/deja-cli/tests/workspace_boundaries.rs) and `unsafe_code = "deny"`:

- **`deja-core`:** Closed-world dependency allowlist — no `rayon`, `cap-std`, or `criterion`. Use `std::thread::scope` I/O prefetching (0 extra crates) while preserving sequential SHA-256 byte-stream parity.
- **`deja-flow`:** Pure platform-neutral logic with zero HTTP, SQLite, WebSocket, or CLI dependencies. Explicit `DriverCapabilities` + `UnsupportedCapability` contracts prevent silent platform emulation.
- **APFS CoW snapshots (`DEJA-79`):** Invoke `/bin/cp -cR` via `tokio::process::Command` rather than `unsafe { libc::clonefile(...) }`.
- **Zero OpenSSL / native-TLS:** All HTTP/TLS uses `reqwest` with `rustls`.
