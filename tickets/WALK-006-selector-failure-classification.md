---
id: WALK-006
title: Classify stale selectors separately from product regressions
milestone: M4A
priority: P0
status: done
depends_on: [WALK-002]
---

# WALK-006 — Classify stale selectors separately from product regressions

## Outcome

Make invalidation repeatable and ensure repair is considered only for selector/navigation failures,
never for a meaningful final product assertion.

## Checklist

- [x] Add a controlled fixture mode/source change that makes only `checkout_button` stale.
- [x] Preserve WALK-001's curated failing evidence and make the experiment reproducible from a clean
  Android build without hand-editing ignored memory.
- [x] Parse runner output into conservative `selector`, `navigation`, `product_assertion`, `runner`,
  `timeout`, and `unknown` failure categories with the failing step and raw artifact reference.
- [x] Treat heuristics as advisory; ambiguous output stays `unknown`, never automatically repairable.
- [x] Keep cache-hit replay at zero model calls, including when it fails.
- [x] Add positive and negative fixtures for each supported classification.
- [x] Do not weaken or rewrite assertions to match changed product behavior.

## Acceptance evidence

Link a test matrix and real Android artifacts showing the stale run fails only at Checkout, while a
wrong order-confirmation assertion is classified as a product assertion and remains a failure.

## Reproducible evidence

### Controlled fixture and real Android failures

[`fixture-mode.sh`](../spikes/001-android-deja/scripts/fixture-mode.sh) toggles one device-local
demo setting. `MainActivity` reads it only when constructing Checkout: stale mode passes `null` for
that button's existing ID, leaving its text/click behavior and all other views unchanged. The APK,
build fingerprint, cached flow, and successful-store identity do not change. No ignored memory is
hand-edited. `fixture-mode.sh stable` removes the setting; the final device value was `null` and its
ADB state remained `device`.

The full clean-build/run sequence is linked in the
[spike README](../spikes/001-android-deja/README.md#repeatable-stale-selector-and-product-assertion-failures-walk-006)
and [Android evidence summary](../spikes/001-android-deja/evidence/walk006-android-evidence.json).
All runs used `walkcache_api_34` / `emulator-5554`, Maestro 2.12.0, APK SHA-256
`47ad33ba3cab8d4c1c268b1522e5ee7fd2d799fc3da6fdc884f99a40b7a166aa`, and a 120-second timeout.

- [Stale-selector report](../spikes/001-android-deja/evidence/walk006-selector-android-report.json)
  and [raw output](../spikes/001-android-deja/evidence/walk006-selector-android-output.log):
  `bun scripts/walkcache.ts run checkout` exited 1 after 39,287 ms. It was a real `cacheStatus: "hit"`
  with `modelCalls: 0`. Exactly 16 preceding steps completed; the sole failure was `Tap on id:
  dev.walkcache.checkout:id/checkout_button`. Classification was advisory `selector`, with the raw
  artifact reference `.walkcache/reports/run-1795329420756/output.log`.
- [Wrong-order product-assertion report](../spikes/001-android-deja/evidence/walk006-product-assertion-android-report.json)
  and [raw output](../spikes/001-android-deja/evidence/walk006-product-assertion-android-output.log):
  `bun scripts/walkcache.ts remember checkout --flow flows/product-regression.yaml` exited 1 after
  42,040 ms with `passed: false`, `cacheStatus: "rejected"`, and zero model calls. All 19 preceding
  steps completed, including Checkout and both confirmation-screen assertions. Only the deliberately
  wrong final `Order #WC-9999 · Mochi sticker pack` assertion failed. It was classified
  `product_assertion`, `repairEligible: false`; the negative fixture was not persisted or repaired.
- [Restored stable report](../spikes/001-android-deja/evidence/walk006-restored-stable-android-report.json):
  the unchanged known-good flow was independently verified again in 25,968 ms, passed with zero model
  calls, and had no failure classification. The failure did not weaken any existing flow assertion.

The evidence summary records SHA-256 values for both raw outputs and for the unchanged WALK-001
`run-summary.json`, original agent flow, repaired flow, and selector-repair diff. Those historical
files have no diff in this ticket.

### Deterministic classification matrix and cache-hit guard

The [test matrix](../spikes/001-android-deja/evidence/walk006-classification-matrix.md) links 12
positive/negative raw-output fixtures across all six categories. Structured runner and timeout signals
take precedence. Selector requires an explicit missing-element diagnostic and recognized action;
navigation requires a recognized launch/open-link failure; product assertion requires a quoted text
assertion plus Maestro's explicit assertion-false diagnostic. Mixed diagnostics, multiple failed
steps, unrecognized output, and ID/assertion ambiguity remain `unknown`. Only selector/navigation are
repair-eligible inputs for later work; this ticket performs no repair or source-patch generation.

The cache orchestration test seeds through the real atomic store, executes the real CLI with a
deterministic fake Maestro failure, and proves the failing hit retains `modelCalls: 0`, the known-good
flow bytes, raw output reference, and failed validation. A subsequent lookup is a `failed-validation`
miss, also with zero model calls. Real Android stale evidence above independently records the same
zero-model cache-hit contract.

### Quality gates

From the repository root:

```sh
bun test packages/maestro/test/failure-classification.test.ts \
  packages/maestro/test/cache-replay-classification.test.ts \
  packages/maestro/test/maestro-runner.test.ts --timeout=10000 --check
bun test --timeout=10000 --check
bun run lint
bun run build
git diff --check
```

[Focused output](../spikes/001-android-deja/evidence/walk006-classification-unit-tests.txt):
22 tests / 83 assertions, 0 failures. [Full output](../spikes/001-android-deja/evidence/walk006-all-tests.txt):
43 tests / 181 assertions, 0 failures, preserving every prior test. [Lint](../spikes/001-android-deja/evidence/walk006-lint.txt)
passed with 0 warnings/errors; [type-check and production build](../spikes/001-android-deja/evidence/walk006-build.txt)
passed; [diff check](../spikes/001-android-deja/evidence/walk006-diff-check.txt) was empty.
Generated `.walkcache/`, `.tmp/`, APK/build, and Maestro debug artifacts remain ignored.

## Non-goals

No model-generated patch in this ticket, no generalized natural-language diagnosis, and no claim that
log parsing proves the root cause of every application failure.

