---
id: WALK-008
title: Build the headless CLI around production plugin contracts
milestone: M6A–M6B
priority: P0
status: done
depends_on: [WALK-003, WALK-004, WALK-005, WALK-006, WALK-007]
---

# WALK-008 — Build the headless CLI around production plugin contracts

## Outcome

Move the proven lifecycle behind small TypeScript interfaces and expose a reliable headless product
without deleting or weakening the Android spike.

## Checklist

- [x] Define core goal, walk, run result, failure, artifact, and metrics contracts independently of
  OpenClaw, Maestro, React, and a particular storage implementation.
- [x] Put `WalkStore`, `WalkRunner`, `WalkExplorer`, `SelectorRepairer`, and `RunReporter` behind
  injected interfaces; shared orchestration contains no per-game/per-demo or per-platform switches.
- [x] Add workspace/package boundaries only as needed by the working vertical slice.
- [x] Provide `walkcache explore <goal>`, `run <goal>`, `remember`, `list`, and `inspect <report>` with
  useful help, configuration, output, and documented exit codes.
- [x] Make `run` check local memory before explorer invocation; cache hits always bypass the model.
- [x] On a miss, use the verified cold lifecycle; on repairable staleness, invoke only WALK-007's
  bounded policy; on regression/unknown failure, report and stop.
- [x] Print cache status, model calls, duration, assertions, repair count, YAML/report paths, and diffs.
- [x] Preserve independent Maestro YAML execution in CI without WalkCache or an LLM.
- [x] Add orchestration tests proving cache hits construct no explorer/model client.

## Acceptance evidence

From documented configuration, link cold, warm, miss, stale-selector repair, and product-regression
CLI transcripts/reports. Run lint, type-check, focused tests, and the original Android flow. Include
a constructor/spy test proving zero explorer invocation on a warm hit.

Evidence:

- Configuration and usage: [`walkcache.config.json`](../../spikes/001-android-walkcache/walkcache.config.json)
  and [`apps/cli/README.md`](../../apps/cli/README.md).
- Cold miss through the production CLI:
  [`transcript`](../../spikes/001-android-walkcache/evidence/walk008-cold-miss.transcript.txt) and
  [`report`](../../spikes/001-android-walkcache/evidence/walk008-cold-miss-report.json) — passed,
  13 model calls, 11 assertions, 95,499 ms, and exit 0. This was the cold discovery path; it is not a
  warm-replay model count.
- Independently verified `remember` and warm replay:
  [`remember transcript`](../../spikes/001-android-walkcache/evidence/walk008-remember.transcript.txt),
  [`warm transcript`](../../spikes/001-android-walkcache/evidence/walk008-warm.transcript.txt), and
  [`warm report`](../../spikes/001-android-walkcache/evidence/walk008-warm-report.json) — warm passed
  with 0 model calls, 12 assertions, 24,662 ms, and exit 0.
- Bounded stale repair:
  [`transcript`](../../spikes/001-android-walkcache/evidence/walk008-stale-repair.transcript.txt),
  [`report`](../../spikes/001-android-walkcache/evidence/walk008-stale-repair-report.json), and
  [`proposed diff`](../../spikes/001-android-walkcache/evidence/walk008-stale-repair.diff) — repaired
  after one attempt, rebuilt and independently verified, 65,865 ms, never auto-committed.
- Genuine product regression:
  [`transcript`](../../spikes/001-android-walkcache/evidence/walk008-product-regression.transcript.txt)
  and [`report`](../../spikes/001-android-walkcache/evidence/walk008-product-regression-report.json) —
  `product_assertion`, 0 model calls, 0 repair attempts, no source diff, and exit 1.
- `list`: [`transcript`](../../spikes/001-android-walkcache/evidence/walk008-list.transcript.txt).
- Constructor/spy regression: `packages/core/test/orchestration.test.ts` proves a warm hit constructs
  neither an explorer nor its model client and constructs no repairer.
- Original Maestro YAML, independent of WalkCache or an LLM:
  [`15-step passing output`](../../spikes/001-android-walkcache/evidence/walk008-original-android-flow.txt).
- Fixture restoration: [`source and device state`](../../spikes/001-android-walkcache/evidence/walk008-fixture-restored.txt).
- Final checks: [`80 tests / 500 assertions`](../../spikes/001-android-walkcache/evidence/walk008-all-tests.txt),
  [`lint`](../../spikes/001-android-walkcache/evidence/walk008-lint.txt),
  [`type-check and production build`](../../spikes/001-android-walkcache/evidence/walk008-build.txt),
  and [`git diff --check`](../../spikes/001-android-walkcache/evidence/walk008-diff-check.txt).

