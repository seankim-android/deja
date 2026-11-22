---
id: WALK-010
title: Prove the same journey on web through its adapter
milestone: M5B
priority: P0
status: done
depends_on: [WALK-005, WALK-007, WALK-008]
---

# WALK-010 — Prove the same journey on web through its adapter

## Outcome

Add web only after Android correctness gates pass, and claim support only from a real Chromium run.

## Checklist

- [x] Build a deterministic secret-free web login → add item → checkout fixture with stable DOM
  selectors and meaningful final assertions.
- [x] Implement selector/build identity through the platform adapter, not shared per-demo conditionals.
- [x] Check the installed Maestro version's actual web support and document beta/Chromium constraints.
- [x] Run cold exploration, independent verification, warm replay, invalidation, and regression tests
  or explicitly narrow the claimed support to the behaviors actually demonstrated.
- [x] Record platform, browser, build, and adapter identity in cache keys and reports.
- [x] Keep Android evidence and commands reproducible after shared changes.

## Acceptance evidence

Link actual Chromium commands, YAML, reports, and supported behavior matrix. If tooling blocks the
run, record the exact error/version and mark web unsupported in demo/docs; shared YAML alone is not
proof of web support.

Evidence:

- [Fixture and supported-behavior matrix](../../spikes/002-web-walkcache/README.md): deliberately
  supports independently reviewed YAML through `remember`, exact Chromium replay, build invalidation,
  and honest regression handling. Model-led cold Web exploration and Web repair are explicitly
  unsupported; no Android behavior is substituted. The cold-boundary command exits 3 with 0 model calls.
- [Installed Maestro 2.12.0 / Chromium capabilities and exact flags](../../spikes/002-web-walkcache/evidence/walk010-maestro-web-support.txt)
  and [the 12-assertion YAML](../../spikes/002-web-walkcache/evidence/checkout.yaml).
- [Independent remember](../../spikes/002-web-walkcache/evidence/walk010-remember.txt): real Chromium
  PASS, 43,660 ms, 12 assertions, 0 model calls, persisted only after verification.
- [Warm replay](../../spikes/002-web-walkcache/evidence/walk010-warm.txt): real Chromium cache HIT,
  42,634 ms, 12 assertions, 0 model calls and 0 repairs. This is not a claim of wall-clock speedup.
- [Exact identity](../../spikes/002-web-walkcache/evidence/walk010-identity.json): platform `web`,
  browser `chromium`, adapter `maestro-web@2.12.0`, static build-tree fingerprint, app, goal and flow schema.
- [Explicit cold boundary](../../spikes/002-web-walkcache/evidence/walk010-cold-unsupported.txt):
  `exploration-rejected`, 0 ms, 0 model calls, exit 3, and no YAML persisted.
- [Build invalidation](../../spikes/002-web-walkcache/evidence/walk010-build-invalidation.txt): changed
  static-tree fingerprint → stale-build miss → repair-rejected in 1 ms with 0 model calls, no cold fallback,
  and the probe restored byte-for-byte.
- [Real product regression](../../spikes/002-web-walkcache/evidence/walk010-product-regression.txt):
  backend changed the exact order to `WC-9999` while frontend bytes/fingerprint remained unchanged;
  cached real Chromium replay failed the meaningful `WC-1001` assertion in 49,185 ms with 0 model calls,
  0 repairs, and `product_assertion`. [Restored verification](../../spikes/002-web-walkcache/evidence/walk010-restored-after-regression.txt)
  independently proves the default fixture again after restoring `WC-1001`.
- [Android replay after shared adapter changes](../../spikes/001-android-walkcache/evidence/walk010-android-warm.txt):
  real cache HIT, 12 assertions, 30,421 ms, 0 model calls and 0 repairs, with the original YAML hash.
- [101 tests / 599 assertions](../../spikes/002-web-walkcache/evidence/walk010-all-tests.txt) and
  [lint, TypeScript and viewer production build](../../spikes/002-web-walkcache/evidence/walk010-lint-build.txt).

