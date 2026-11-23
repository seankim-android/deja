---
id: WALK-007
title: Enforce bounded repair and the product-regression guard
milestone: M4B–M4C
priority: P0
status: done
depends_on: [WALK-006]
---

# WALK-007 — Enforce bounded repair and the product-regression guard

## Outcome

Productionize the validated two-file selector patch under strict limits and complete the missing
negative experiment: a genuine product regression must stay red.

## Checklist

- [x] Permit repair only for WALK-006's explicitly repairable category; reject product assertions,
  ambiguous failures, timeouts, and infrastructure errors.
- [x] Allow writes only under configured source roots and permitted file types; reject path escapes.
- [x] Limit repair to one attempt, one time budget, and the affected component/selector files.
- [x] Display the proposed diff and affected files; do not auto-commit or create a pull request.
- [x] Rebuild, recompute build identity, and verify the full journey before replacing cached memory.
- [x] Preserve the previous flow, failed attempts, patch, build report, and verification artifacts.
- [x] Add a deterministic product-regression fixture (for example, incorrect final total/confirmation).
- [x] Prove the repair path neither changes source nor rewrites assertions for that fixture and exits
  nonzero with the last known-good memory intact.

## Acceptance evidence

Link the bounded successful repair report and diff plus the negative regression report, unchanged
source diff, and preserved-memory check. Retain WALK-001's original repair as historical evidence;
do not represent it as proof of generalized autonomous repair.

## Reproducible evidence

The exact build/install, experiment-only source reversal, repair, regression, and restoration commands
are in the [spike README](../spikes/001-android-deja/README.md#bounded-source-repair-and-genuine-regression-guard-walk-007).
All real runs used Android API 34 / `walkcache_api_34` / `emulator-5554`, Maestro 2.12.0, the unchanged
`evidence/repaired-checkout.yaml` journey, and one 240,000 ms monotonic lifecycle budget. The repair
uses no model process. [`packages/repair/README.md`](../packages/repair/README.md) documents the strict
eligibility, canonical-write, deadline, rollback, artifact, and no-Git contracts.

### Bounded successful selector/source repair

- [Repair report](../spikes/001-android-deja/evidence/walk007-repair-report.json): status
  `repaired`, one attempt, 65,935 ms overall, no auto-commit/PR, exact two affected files, zero model
  calls, and atomic verified persistence. [The actual minimal diff](../spikes/001-android-deja/evidence/walk007-repair.diff)
  and [plan](../spikes/001-android-deja/evidence/walk007-repair-plan.json) touch only
  `MainActivity.java` and `ids.xml`.
- [Selector diagnosis](../spikes/001-android-deja/evidence/walk007-repair-diagnosis-output.log)
  failed only at Checkout in 39,408 ms after every preceding step completed. [Gradle and adb install output](../spikes/001-android-deja/evidence/walk007-repair-build-output.log)
  passed in 911 ms. APK SHA-256 changed from
  `b991a1796d81f390d9b9fd00853986fe066431bdc8f9fcb5dfce78d755e3f91a` to
  `1de8864ea046b64d124ece93ca97b0ae69d11d1fc3d39a89ff405f0e2dd2262d`.
- [Independent full-journey verification](../spikes/001-android-deja/evidence/walk007-repair-verification-output.log)
  passed in 25,601 ms. Only then did atomic replacement occur. [Previous metadata](../spikes/001-android-deja/evidence/walk007-repair-previous-memory.json),
  [replacement metadata](../spikes/001-android-deja/evidence/walk007-repair-stored-memory.json), and
  all previous/diagnosis/build/verification artifacts remain under the attempt directory. Flow SHA-256
  stayed `e09660f45502b720f79f95a65c78ad5f48a4e11ae1ff5f52c6cb99972b12b478`.
- [Verified replacement warm replay](../spikes/001-android-deja/evidence/walk007-repair-warm-report.json)
  passed in 24,129 ms with a cache hit, the recomputed build fingerprint, and zero model calls.

### Genuine app-behavior regression and preservation

`scripts/fixture-mode.sh product-regression` makes `MainActivity.showConfirmation()` actually display
`Order #WC-9999 · Mochi sticker pack`; every selector and all other checkout behavior stays unchanged.
The cached flow still asserts `Order #WC-1001 · Mochi sticker pack`.

- [Negative repair report](../spikes/001-android-deja/evidence/walk007-negative-report.json):
  nonzero CLI exit 1, status `rejected`, category `product_assertion`, 41,876 ms overall, zero repair
  attempts, and `build`, `verification`, persistence, and proposed patch all absent.
- [Raw negative diagnosis](../spikes/001-android-deja/evidence/walk007-negative-diagnosis-output.log)
  completes Checkout and fails only the unchanged final product assertion after 41,872 ms.
- [Preservation proof](../spikes/001-android-deja/evidence/walk007-negative-preservation.json):
  memory SHA-256 stayed
  `b9f2118712b22e2b68228715cdd408bda8a25d7a9dbf743299ef57f081cd2f78`; flow SHA-256 stayed
  `e09660f45502b720f79f95a65c78ad5f48a4e11ae1ff5f52c6cb99972b12b478`; byte comparisons passed.
  The [unchanged app-source diff](../spikes/001-android-deja/evidence/walk007-negative-source.diff)
  is exactly zero bytes. No assertion or source file was rewritten.
- [Restored stable report](../spikes/001-android-deja/evidence/walk007-restored-stable-report.json)
  passed the complete unchanged journey in 25,653 ms with zero model calls.
  [Fixture restoration](../spikes/001-android-deja/evidence/walk007-fixture-restored.txt)
  records both settings `null`, the expected APK fingerprint, and continuing emulator state `device`.

### Failed-attempt containment during implementation

Two superseded implementation attempts stayed red and were never used as passing evidence. Both
restored source and retained memory byte-for-byte; their original ignored directories are retained:

- [Attempt 01](../spikes/001-android-deja/evidence/walk007-failed-attempt-01-report.json): 80,162 ms;
  the rebuilt APK was not installed, so [verification](../spikes/001-android-deja/evidence/walk007-failed-attempt-01-verification-output.log)
  remained at the selector in 39,619 ms and persistence was skipped. This led to the bounded adb-install
  step and its focused regression tests.
- [Attempt 02](../spikes/001-android-deja/evidence/walk007-failed-attempt-02-report.json): 40,350 ms;
  an unchanged local rebuilt fingerprint was rejected before verification/persistence. Rebuilding the
  stale source restored a truthful stale fingerprint before the accepted run.
- [Attempt locations and previous-memory hashes](../spikes/001-android-deja/evidence/walk007-failed-attempts.txt).

### Focused and full quality gates

From the repository root:

```sh
bun test packages/repair/test packages/store-local/test/local-store.test.ts --timeout=10000 --check
bun test --timeout=10000 --check
bun run lint
bun run build
git diff --check
```

Recorded results: [36 focused tests / 228 assertions](../spikes/001-android-deja/evidence/walk007-repair-unit-tests.txt),
[66 full-suite tests / 382 assertions](../spikes/001-android-deja/evidence/walk007-all-tests.txt),
[zero lint warnings/errors](../spikes/001-android-deja/evidence/walk007-lint.txt),
[passing `tsc -b && vite build`](../spikes/001-android-deja/evidence/walk007-build.txt), and
[empty diff check](../spikes/001-android-deja/evidence/walk007-diff-check.txt). The focused suite
covers every refusal category, other selectors, canonical roots/types/path and symlink escapes,
one attempt/shared timeout, rollback, no Git invocation, build-plus-install behavior, changed build
identity, atomic verified replacement, and negative byte preservation. Generated `.walkcache/`, APK,
build, temporary, and command artifacts remain ignored.

The successful experiment restored both selector files to their exact pre-experiment repaired bytes;
`git diff` shows only the intentional app-behavior regression fixture in app source. WALK-001's
curated summary, flow, repaired flow, and original diff have no changes. This ticket demonstrates only
the narrow deterministic Checkout repair, not generalized autonomous repair.

