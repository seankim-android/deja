# WalkCache deterministic native iOS fixture

A minimal SwiftUI login → add item → checkout app used only by WALK-011. It shares no product code
with Blankaday or the Android/Web fixtures. Both fields accept only deterministic test values.

## Reproduce the verified path

Prerequisites: macOS with Xcode 27.2, the iOS 27.0 simulator runtime, Bun 1.4+, and Maestro 2.12.0.
Other versions and physical devices are not covered by this evidence. All commands below run from
`spikes/003-ios-walkcache`.

Create and boot a dedicated simulator if `walkcache_ios` does not already exist:

```sh
xcrun simctl create walkcache_ios \
  com.apple.CoreSimulator.SimDeviceType.iPhone-17 \
  com.apple.CoreSimulator.SimRuntime.iOS-27-0
# Put the returned UDID in walkcache.config.json's device field when recreating the fixture.
xcrun simctl boot <created-UDID>
xcrun simctl bootstatus <created-UDID> -b
```

Never use `simctl shutdown all` or another worktree's simulator. Build/install only this fixture on
the configured `walkcache_ios` UDID:

```sh
xcodebuild -project WalkCacheFixture.xcodeproj -scheme WalkCacheFixture \
  -configuration Debug -sdk iphonesimulator -derivedDataPath build \
  CODE_SIGNING_ALLOWED=NO build
xcrun simctl install <walkcache_ios-UDID> \
  build/Build/Products/Debug-iphonesimulator/WalkCacheFixture.app
```

The checked-in evidence used UDID `DFC4D233-498B-4B33-B713-E59B239FA86A`, Xcode 27.2, iOS 27.0,
and Maestro 2.12.0. The Android fixture's environment helper may be sourced solely to put the
installed Maestro/Java executables on `PATH`:

```sh
. ../001-android-walkcache/scripts/env.sh

# Standalone independent Maestro YAML: no WalkCache or model required.
maestro --no-ansi --platform ios --device <walkcache_ios-UDID> test evidence/checkout.yaml

# Independently execute reviewed YAML; persist only after the full journey passes.
bun ../../apps/cli/walkcache.ts remember checkout \
  --flow evidence/checkout.yaml --config walkcache.config.json

# Exact cache hit: direct iOS Maestro replay, 0 model calls.
bun ../../apps/cli/walkcache.ts run checkout --config walkcache.config.json

# Read the redacted report.
bun ../../apps/cli/walkcache.ts inspect .walkcache/reports/latest.json \
  --config walkcache.config.json
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
  defaults write NSGlobalDomain WALKCACHE_FIXTURE_ORDER -string WC-9999
bun ../../apps/cli/walkcache.ts run checkout --config walkcache.config.json
# Expected: product-regression on the unchanged WC-1001 assertion; no repair.
xcrun simctl spawn <walkcache_ios-UDID> defaults delete NSGlobalDomain WALKCACHE_FIXTURE_ORDER
```

The build invalidation transcript uses a generated `PkgInfo` probe and proves its bytes were restored.
Raw `.walkcache`, `build`, simulator state, and attempt logs stay ignored. Committed evidence contains
only the reviewed YAML, redacted structured reports, and concise command/result transcripts.

## Android compatibility limitation

The requested post-change Android replay could not start because the existing `walkcache_api_34`
emulator rejected boot with `Your device does not have enough disk space` (1.6 GiB observed free).
This is recorded in `evidence/walk011-android-compatibility.txt`; portable tests are not presented as
Android device proof. No existing Android evidence was changed.

See `evidence/walk011-toolchain.txt`, `walk011-direct-ios.txt`, lifecycle reports/transcripts, and the
105-test / 621-assertion portable suite for exact commands and results.
