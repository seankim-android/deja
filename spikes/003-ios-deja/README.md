# Déjà deterministic native iOS fixture

> **Historical prototype record:** The `bun ../../apps/cli/deja.ts` and Maestro 2.12 commands below document the retired WALK-011 prototype (reproducible at pre-`DEJA-75` commit `fedb4e5`). For the current pure-Rust native iOS Simulator spike (`DejaXCTestRunner`, zero Maestro/Bun/Java), see [SPIKE_EVIDENCE.md](../../drivers/ios/DejaXCTestRunner/SPIKE_EVIDENCE.md).

A minimal SwiftUI login → add item → checkout app used by WALK-011 and the native Rust iOS spike (`DEJA-60`). It shares no product code
with Blankaday or the Android/Web fixtures. Both fields accept only deterministic test values.

## Reproduce the verified path

Prerequisites: macOS with Xcode 27.2, the iOS 27.0 simulator runtime, Bun 1.4+, and Maestro 2.12.0.
Other versions and physical devices are not covered by this evidence. All commands below run from
`spikes/003-ios-deja`.

Create and boot a dedicated simulator if `walkcache_ios` does not already exist:

```sh
xcrun simctl create walkcache_ios \
  com.apple.CoreSimulator.SimDeviceType.iPhone-17 \
  com.apple.CoreSimulator.SimRuntime.iOS-27-0
# Put the returned UDID in deja.config.json's device field when recreating the fixture.
xcrun simctl boot <created-UDID>
xcrun simctl bootstatus <created-UDID> -b
```

Never use `simctl shutdown all` or another worktree's simulator. Build/install only this fixture on
the configured `walkcache_ios` UDID:

```sh
xcodebuild -project DejaFixture.xcodeproj -scheme DejaFixture \
  -configuration Debug -sdk iphonesimulator -derivedDataPath build \
  CODE_SIGNING_ALLOWED=NO build
xcrun simctl install <walkcache_ios-UDID> \
  build/Build/Products/Debug-iphonesimulator/DejaFixture.app
```

The checked-in evidence used UDID `DFC4D233-498B-4B33-B713-E59B239FA86A`, Xcode 27.2, iOS 27.0,
and Maestro 2.12.0. The Android fixture's environment helper may be sourced solely to put the
installed Maestro/Java executables on `PATH`:

```sh
. ../001-android-deja/scripts/env.sh

# Standalone independent Maestro YAML: no Déjà or model required.
maestro --no-ansi --platform ios --device <walkcache_ios-UDID> test flows/checkout.yaml

# Independently execute reviewed YAML; persist only after the full journey passes.
bun ../../apps/cli/deja.ts remember checkout \
  --flow flows/checkout.yaml --config deja.config.json

# Exact cache hit: direct iOS Maestro replay, 0 model calls.
bun ../../apps/cli/deja.ts run checkout --config deja.config.json

# Read the redacted report.
bun ../../apps/cli/deja.ts inspect .deja/reports/latest.json \
  --config deja.config.json
```

Expected verified journey: 12 assertions, ending with `Order #WC-1001 · Mochi sticker pack`.
Stable accessibility identifiers name every screen and action. The app also asserts visible product,
quantity, total, confirmation, and exact order text; IDs alone are not treated as the product outcome.

## Verified support boundary

| Behavior | WALK-011 status |
| --- | --- |
| Real iPhone 17 / iOS 27.0 simulator login → add item → checkout with 12 assertions | Supported and verified |
| Independently reviewed YAML through `remember` | Supported and verified |
| Exact warm replay without a model call | Supported and verified |
| Cache partition by goal/app/platform/flow schema/adapter/native `.app` build tree | Supported and verified |
| Native build-tree change | Invalidates; repair is rejected, with no blind cold fallback |
| Actual changed order response with unchanged build/YAML | Meaningful `product_assertion`; no repair or weakened assertion |
| Cold model-led iOS exploration | **Unsupported**: exits as rejected with 0 model calls; use reviewed YAML + `remember` |
| iOS selector/source repair | **Unsupported**: Android repair is never reused for iOS |
| Physical devices, other simulator models/runtimes, other Maestro versions, or parallel sharding | Not claimed |

The adapter is `maestro-ios@2.12.0`. It fingerprints relative paths and contents of every regular
file in the `.app` bundle, rejects symbolic links/special files within the tree, selects
`--platform ios --device <UDID>`, and contributes adapter identity to exact cache lookup. Platform,
bundle ID, simulator, build fingerprint, adapter, YAML checksum, and validation outcomes remain in
reports. No speedup is claimed from the recorded remember/warm durations.

For the regression proof only, the fixture reads an optional simulator-global order override. Set it
only on `walkcache_ios`, run the cached flow, then always delete it and independently `remember` the
default fixture again:

```sh
xcrun simctl spawn <walkcache_ios-UDID> \
  defaults write NSGlobalDomain DEJA_FIXTURE_ORDER -string WC-9999
bun ../../apps/cli/deja.ts run checkout --config deja.config.json
# Expected: product-regression on the unchanged WC-1001 assertion; no repair.
xcrun simctl spawn <walkcache_ios-UDID> defaults delete NSGlobalDomain DEJA_FIXTURE_ORDER
```

## First-launch crash mode (DEJA-7)

For the rerun evidence only, the fixture can crash on its first launch and start normally after.
It is off unless the simulator-global default `DEJA_FIXTURE_CRASH_MARKER` names a file path outside
the app container (the flow's `clearState: true` wipes the container). The first launch finds no
file, creates it and crashes; later launches start normally. Set it only on `walkcache_ios`, run,
then always delete both the default and the marker:

```sh
xcrun simctl spawn <walkcache_ios-UDID> \
  defaults write NSGlobalDomain DEJA_FIXTURE_CRASH_MARKER -string /tmp/deja-fixture-crash-marker
rm -f /tmp/deja-fixture-crash-marker
bun ../../apps/cli/deja.ts run checkout --rerun-failed --summary --config deja.config.json
# Expected: outcome flaky (exit 7), attempt 1 app_crash, attempt 2 passed.
# A real assertion failure (the DEJA_FIXTURE_ORDER override above) still reports one steady
# product_assertion failure with --rerun-failed.
xcrun simctl spawn <walkcache_ios-UDID> defaults delete NSGlobalDomain DEJA_FIXTURE_CRASH_MARKER
rm -f /tmp/deja-fixture-crash-marker
```

When you run this, record which step fails after the crash. If it is an `assertVisible`, the run
may be labeled `product_assertion` instead of `app_crash`, because Déjà does not wait for the crash
report after a failed assertion. That is a known limit; note the result in the ticket.

Status: the mode is built and the Déjà side is covered by tests with fakes. It has **not** been run
on a simulator yet (the dedicated simulator was in use), so there is no recorded evidence.

The build invalidation transcript uses a generated `PkgInfo` probe and proves its bytes were restored.
Raw `.deja`, `build`, simulator state, and attempt logs stay ignored. Committed evidence contains
only the reviewed YAML, redacted structured reports, and concise command/result transcripts.

## Android compatibility limitation

The requested post-change Android replay could not start because the existing `walkcache_api_34`
emulator rejected boot with `Your device does not have enough disk space` (1.6 GiB observed free).
This is recorded in `evidence/walk011-android-compatibility.txt`; portable tests are not presented as
Android device proof. No existing Android evidence was changed.

See `evidence/walk011-toolchain.txt`, `walk011-direct-ios.txt`, lifecycle reports/transcripts, and the
105-test / 621-assertion portable suite for exact commands and results.
