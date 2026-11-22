---
id: WALK-011
title: Prove the same journey on iOS through its adapter
milestone: M5C
priority: P0
status: done
depends_on: [WALK-005, WALK-007, WALK-008]
---

# WALK-011 — Prove the same journey on iOS through its adapter

## Outcome

Validate iOS independently without delaying or weakening the Android proof.

## Checklist

- [x] Use the smallest deterministic native SwiftUI fixture that supports login → add item → checkout.
- [x] Add stable accessibility identifiers and 12 meaningful assertions, including the exact final order.
- [x] Borrow only general ID-based Maestro conventions; use no Blankaday product flows, credentials,
  personal data, or app code.
- [x] Implement platform, native `.app` build fingerprinting, and adapter identity through the execution
  adapter; create and use only the dedicated `walkcache_ios` simulator.
- [x] Prove standalone YAML, independent `remember`, exact zero-model warm replay, build invalidation,
  and a genuine unchanged-build product regression; explicitly leave cold exploration and repair
  unsupported rather than substituting Android behavior.
- [x] Attempt Android compatibility after shared changes and record its concrete pre-boot disk-space
  blocker; no post-change Android device pass is claimed.

## Acceptance evidence

The [fixture and supported-behavior matrix](../spikes/003-ios-walkcache/README.md) limits support to
real iPhone 17 / iOS 27.0 simulator execution with Xcode 27.2 and Maestro 2.12.0. Cold model-led iOS
exploration and iOS repair are explicitly unsupported. Physical devices, other runtimes/models, and
other Maestro versions are not claimed.

- [Toolchain, exact commands, native build, simulator/runtime, adapter/build/YAML identities](../spikes/003-ios-walkcache/evidence/walk011-toolchain.txt).
- [Independent standalone iOS YAML execution](../spikes/003-ios-walkcache/evidence/walk011-direct-ios.txt)
  and the [12-assertion reviewed YAML](../spikes/003-ios-walkcache/evidence/checkout.yaml): every step
  completed in genuine Maestro iOS execution, without WalkCache or a model.
- [Independent remember](../spikes/003-ios-walkcache/evidence/walk011-remember.txt) and its
  [redacted result report](../spikes/003-ios-walkcache/evidence/walk011-remember-report.json): PASS,
  29,067 ms, 12 assertions, 0 model calls; verified before persistence.
- [Exact warm replay](../spikes/003-ios-walkcache/evidence/walk011-warm.txt) and its
  [redacted result report](../spikes/003-ios-walkcache/evidence/walk011-warm-report.json): cache HIT,
  31,258 ms, 12 assertions, 0 model calls and 0 repairs. No wall-clock speedup is claimed.
- [Explicit cold boundary](../spikes/003-ios-walkcache/evidence/walk011-cold-unsupported.txt) and
  [report](../spikes/003-ios-walkcache/evidence/walk011-cold-unsupported-report.json):
  `exploration-rejected` in 0 ms with 0 model calls, exit 3, and no persisted YAML.
- [Build invalidation](../spikes/003-ios-walkcache/evidence/walk011-build-invalidation.txt) and
  [report](../spikes/003-ios-walkcache/evidence/walk011-build-invalidation-report.json): changed
  `.app`-tree fingerprint → stale-build miss → repair-rejected in 1 ms with 0 model calls and no cold
  fallback. The generated probe file was restored byte-for-byte.
- [Real product regression](../spikes/003-ios-walkcache/evidence/walk011-product-regression.txt) and
  [report](../spikes/003-ios-walkcache/evidence/walk011-product-regression-report.json): only the
  simulator-global fixture order changed to `WC-9999`; cached genuine iOS replay failed the unchanged
  `WC-1001` assertion in 47,704 ms with `product_assertion`, 0 model calls, and 0 repairs.
  [Restored verification](../spikes/003-ios-walkcache/evidence/walk011-restored.txt) and its
  [report](../spikes/003-ios-walkcache/evidence/walk011-restored-report.json) independently pass the
  default fixture after deleting the override (21,666 ms, 12 assertions).
- [Android compatibility attempt](../spikes/003-ios-walkcache/evidence/walk011-android-compatibility.txt):
  the existing `walkcache_api_34` emulator exited before boot because insufficient disk space remained
  (1.6 GiB observed free). No post-change Android device compatibility is claimed and no unrelated
  files were deleted to bypass the environmental gate.
- [105 tests / 621 assertions](../spikes/003-ios-walkcache/evidence/walk011-all-tests.txt) and
  [lint, TypeScript/viewer build, and diff checks](../spikes/003-ios-walkcache/evidence/walk011-lint-build.txt)
  all pass. Web adapter CLI integration remains green; the Android device replay remains environmentally
  unverified as described above.
