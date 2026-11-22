---
id: WALK-004
title: Add local-store regression and atomicity tests
milestone: M3A–M3C
priority: P0
status: done
depends_on: [WALK-002]
---

# WALK-004 — Add local-store regression and atomicity tests

## Outcome

Make cache persistence conservative and testable so corruption, stale builds, or interrupted writes
never become false passes or destroy reusable memory.

## Checklist

- [x] Test exact goal normalization plus app, platform, schema, flow-checksum, and build identity.
- [x] Cover first-run miss, valid hit, stale build, wrong app/platform, and missing flow.
- [x] Cover malformed JSON, unsupported schema, invalid entries, and flow path escapes.
- [x] Simulate interruption before rename and prove temporary files are not loaded as memory.
- [x] Prove failed verification/replacement preserves the previous known-good flow and metadata.
- [x] Preserve validation history without treating a failed validation as a successful walk.
- [x] Use isolated temporary directories and deterministic fixtures; require no emulator for unit tests.
- [x] Document recovery behavior: report invalid memory and require exploration rather than silently
  converting corruption into a pass.

## Reproducible evidence

### Focused store regressions

From the repository root:

```sh
bun test packages/store-local/test/local-store.test.ts --timeout=10000 --check \
  2>&1 | tee spikes/001-android-walkcache/evidence/local-store-unit-tests.txt
```

[`local-store-unit-tests.txt`](../spikes/001-android-walkcache/evidence/local-store-unit-tests.txt)
records **17 tests, 54 assertions, 0 failures**. Every acceptance case has a named passing test:

- first-run miss; normalized valid hit with exact app/platform/flow-schema/build/checksum identity;
  stale build; wrong app; wrong platform; missing flow; and changed-flow checksum rejection;
- malformed JSON; unsupported memory schema; invalid entries without filtering; lexical flow path
  escape; and canonical symlink escape;
- interruption before flow rename with a surviving ignored temp and no memory; failed metadata
  replacement with ignored orphan/temp and byte-identical known-good flow plus metadata; and failed
  candidate verification with no rename and byte-identical storage;
- failed validation retained after earlier success, followed by a `failed-validation` miss rather
  than a reusable hit; and conservative valid WALK-003 schema-1 upgrade without inventing success.

Tests create and clean isolated OS temporary directories and inject only deterministic filesystem
rename failures. No emulator, Maestro process, OpenClaw session, model, or shared cache is required.
New flows are content-addressed before the atomic metadata rename, so failed replacement does not
overwrite the prior good flow. [`packages/store-local/README.md`](../packages/store-local/README.md)
documents identity, schema/history, invalid-memory reporting, quarantine, and the required WALK-003
`explore` recovery. Corruption never becomes a pass or a silently filtered hit.

### Full regression and static gates

From the repository root:

```sh
bun test --timeout=10000 --check 2>&1 | tee spikes/001-android-walkcache/evidence/local-store-all-tests.txt
bun run lint 2>&1 | tee spikes/001-android-walkcache/evidence/local-store-lint.txt
bun run build 2>&1 | tee spikes/001-android-walkcache/evidence/local-store-build.txt
git diff --check
```

Results: [`all tests`](../spikes/001-android-walkcache/evidence/local-store-all-tests.txt) passed
**27 tests / 151 assertions**, including all 6 unchanged WALK-002 runner tests and all 4 unchanged
WALK-003 cold-lifecycle tests; [`lint`](../spikes/001-android-walkcache/evidence/local-store-lint.txt)
passed with 0 warnings/errors; [`tsc -b && vite build`](../spikes/001-android-walkcache/evidence/local-store-build.txt)
passed; and [`git diff --check`](../spikes/001-android-walkcache/evidence/local-store-diff-check.txt)
reported no errors. Generated `.walkcache/`, `.tmp/`, build, and test-temp artifacts remain ignored.

The production CLI/store/WALK-002 connection also passed a no-emulator smoke from
`spikes/001-android-walkcache`:

```sh
WALKCACHE_MAESTRO="$PWD/../../packages/maestro/test/fixtures/fake-maestro.cjs" \
WALKCACHE_DEVICE=test-device bun scripts/walkcache.ts run checkout
```

It replayed the existing verified WALK-003 YAML through the real WALK-002 adapter, upgraded its valid
schema-1 metadata, retained exploration metrics, appended a passing history entry, and reported
`cacheStatus: "hit"`, `passed: true`, `modelCalls: 0`, flow SHA-256
`064e0621edf6c3e538fc717d71fca1395dd4cf32f09ec1e8a87f002959748f08`, and APK SHA-256
`4746296fa8aa84ae85761915f651b1488c38ef8f5f4e89af0e9ad19784cfcb05`. The fake runner's duration is
not an Android performance measurement.

### Retained real Android stale-build integration

Curated [`WALK-001 run-summary.json`](../spikes/001-android-walkcache/evidence/run-summary.json)
remains unchanged. Its `repair` evidence records the changed real APK forcing
`postRebuildOldCacheStatus: "miss"` with exit code 2 before any replacement, followed by independent
verification in 25,784 ms. The original warm build was
`d1a45c22dd29c1841d121f12e40ea980bb0f6269e5d3110f995444a37c69134e`; the repaired build was
`4746296fa8aa84ae85761915f651b1488c38ef8f5f4e89af0e9ad19784cfcb05`. This is the retained real
Android stale-build integration evidence; no completed cold exploration or redundant Android run was
repeated for WALK-004.

## Non-goals

No cold/warm benchmarking, failure classification, CLI/dashboard expansion, or additional platform
support is claimed. Those remain in their own tickets.

