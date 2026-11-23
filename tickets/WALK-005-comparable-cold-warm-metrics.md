---
id: WALK-005
title: Capture comparable cold and warm metrics
milestone: M3C
priority: P0
status: done
depends_on: [WALK-003, WALK-004]
---

# WALK-005 — Capture comparable cold and warm metrics

## Outcome

Measure the complete cold and warm lifecycle under equivalent conditions, while keeping model
avoidance separate from execution speed.

## Checklist

- [x] Define cold start/end boundaries including exploration, verification, persistence, and device
  setup when it is part of the command.
- [x] Run cold and warm against the same build, goal, device state, and meaningful assertions.
- [x] Record wall time, runner time, MCP calls, model calls, cache status, and verification result.
- [x] Obtain model-call counts from actual runtime instrumentation; never equate MCP calls with model
  calls or fill missing values with zero.
- [x] Repeat enough controlled runs to identify setup noise; report raw samples, not only a best case.
- [x] Save a side-by-side JSON/Markdown artifact suitable for the demo.
- [x] Keep the existing 24.824s and 24.510s warm samples as historical evidence, not a speedup claim.

## Acceptance evidence

The demo-ready artifacts are [`walk005-comparison.md`](../spikes/001-android-deja/evidence/walk005-comparison.md)
and [`walk005-comparison.json`](../spikes/001-android-deja/evidence/walk005-comparison.json).
The JSON embeds every raw sample and exact boundary; the individual reports linked below are the raw
per-command evidence. All times below are milliseconds, and all eight samples passed the unchanged
exact `Order #WC-1001 · Mochi sticker pack` outcome assertion.

### Exact comparable boundaries

**Shared preconditions:** the installed APK, emulator, goal, app, and stable selector fixture were
established before the series and held constant. Emulator boot, Gradle build, APK installation, and
fixture-mode selection are outside both timers. [`walk005-device-conditions.txt`](../spikes/001-android-deja/evidence/walk005-device-conditions.txt)
records `walkcache_api_34` / `emulator-5554`, Android 14/API 34, Maestro 2.12.0, and APK SHA-256
`47ad33ba3cab8d4c1c268b1522e5ee7fd2d799fc3da6fdc884f99a40b7a166aa`. Every raw report independently
records that build fingerprint and device. Both cold candidates and their paired warm flows used
`launchApp clearState: true`.

- **Cold start:** immediately before spawning `bun scripts/walkcache.ts explore checkout --config
  explore.config.json`.
- **Cold end:** after that process exits, including its final report write.
- **Cold included:** Bun/CLI startup, config and APK fingerprinting, attempt setup, exploration-phase
  `adb pm clear`, the complete OpenClaw/Kimi exploration and MCP activity, candidate authoring,
  verification-phase `adb pm clear`, independent WALK-002 clearState execution, atomic persistence,
  and report writing.
- **Warm start:** immediately before spawning `bun scripts/walkcache.ts run checkout`.
- **Warm end:** after that process exits, including validation-history persistence and final report write.
- **Warm included:** Bun/CLI startup, APK fingerprinting, exact lookup and flow loading, the WALK-002
  Maestro process and its clearState launch, validation-history persistence, and report writing.
  OpenClaw and Maestro MCP are never spawned on this direct-hit path.

The outer command-wall timers are therefore equivalent. Internal timers remain separately labeled:
cold `runReport.durationMs` covers the internal lifecycle, cold verification has wrapper and nested
runner values, and warm `runReport.durationMs` is runner-only. `scripts/measure-sample.ts` adds the
outer monotonic wall timer without changing the existing lifecycle or assertions. Cold runs began
with a `not-found` cache miss; each warm began with an exact hit and replayed the flow persisted by
its paired cold run.

### All controlled real Android samples

| Raw report | Pair/mode | Command wall | Nested runner | MCP calls | Model calls | Cache/result |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| [`cold-01`](../spikes/001-android-deja/evidence/walk005-sample-cold-01.json) | pair-1 cold | 96,997 | 25,215 | 11 | 13 | recorded / PASS |
| [`cold-02`](../spikes/001-android-deja/evidence/walk005-sample-cold-02.json) | pair-2 cold | 96,518 | 25,796 | 12 | 13 | recorded / PASS |
| [`warm-01`](../spikes/001-android-deja/evidence/walk005-sample-warm-01.json) | pair-1 warm | 24,461 | 24,442 | 0 | 0 | hit / PASS |
| [`warm-02`](../spikes/001-android-deja/evidence/walk005-sample-warm-02.json) | pair-1 warm | 24,309 | 24,288 | 0 | 0 | hit / PASS |
| [`warm-03`](../spikes/001-android-deja/evidence/walk005-sample-warm-03.json) | pair-1 warm | 25,589 | 25,568 | 0 | 0 | hit / PASS |
| [`warm-04`](../spikes/001-android-deja/evidence/walk005-sample-warm-04.json) | pair-2 warm | 25,112 | 25,094 | 0 | 0 | hit / PASS |
| [`warm-05`](../spikes/001-android-deja/evidence/walk005-sample-warm-05.json) | pair-2 warm | 25,864 | 25,845 | 0 | 0 | hit / PASS |
| [`warm-06`](../spikes/001-android-deja/evidence/walk005-sample-warm-06.json) | pair-2 warm | 25,146 | 25,127 | 0 | 0 | hit / PASS |

Cold internal lifecycle times were 96,981 and 96,502 ms, exploration times were 71,572 and 70,495 ms,
and independent verification wrappers were 25,217 and 25,797 ms. The raw reports retain those nested
values, timestamps, command exit codes, clean-state flags, flow hashes, assertion counts, runner output
references, and persistence details. Warm assertion counts are 11 for pair 1 and 14 for pair 2,
matching the flow authored by each cold exploration; both pairs preserve the same meaningful final
product assertion rather than a weakened common denominator.

Median command wall was **96,757.5 ms cold** (range 96,518–96,997) versus **25,129 ms warm** (range
24,309–25,864), an observed cold-minus-warm median difference of **71,628.5 ms**. Mean wall was 96,757.5
ms cold and 25,080.167 ms warm. Nested runner median was **25,505.5 ms cold** (range 25,215–25,796)
versus **25,110.5 ms warm** (range 24,288–25,845), only a 395 ms median difference with overlapping
ranges. Cold exploration ranged across 1,077 ms and warm wall across 1,555 ms. Thus this controlled
series supports an end-to-end command wall reduction for the sampled cold workflow, **not a claim that
Maestro execution itself becomes dramatically faster or a universal speedup guarantee**. Model
avoidance is independently proven: 13 actual OpenClaw assistant rounds per cold run versus zero on all
warm runs.

Cold MCP counts come only from proxied `tools/call` requests. Cold model counts were read and checked
against each raw OpenClaw `assistantTurns` envelope; each raw report includes the instrumentation path
and SHA-256 without serializing its credential, prompt, or response. Warm zeroes come from the direct
cache-hit execution contract, not missing instrumentation. MCP calls are never treated as model calls.

The historical **24.824 s / 24.510 s** WALK-001 samples remain unchanged in
[`run-summary.json`](../spikes/001-android-deja/evidence/run-summary.json). They are explicitly
marked historical-only and excluded from every new aggregate.

### Deterministic aggregation and quality gates

`packages/metrics` rejects differing builds/devices/goals and weakened outcome assertions, retains
failed samples without turning them into speed claims, propagates unknown instrumentation as unknown,
and reports min/max/range/mean/median plus all raw values. `scripts/compare-metrics.ts` regenerates the
curated per-sample JSON and side-by-side artifacts from ignored raw measurements.

From the repository root:

```sh
bun test packages/metrics/test/cold-warm.test.ts --timeout=10000 --check
bun test --timeout=10000 --check
bun run lint
bun run build
git diff --check
```

Evidence: [focused tests](../spikes/001-android-deja/evidence/walk005-metrics-unit-tests.txt)
(4 tests / 27 assertions), [full suite](../spikes/001-android-deja/evidence/walk005-all-tests.txt)
(**47 tests / 208 assertions**, including all 43 previous tests), [lint](../spikes/001-android-deja/evidence/walk005-lint.txt)
(0 warnings/errors), [type-check/build](../spikes/001-android-deja/evidence/walk005-build.txt),
[evidence regeneration validation](../spikes/001-android-deja/evidence/walk005-evidence-validation.txt),
and [diff check](../spikes/001-android-deja/evidence/walk005-diff-check.txt). Generated
`.walkcache/`, `.tmp/`, APK/build, raw command output, and Maestro artifacts remain ignored. Curated
reports contain no credentials or environment dumps.

[`walk005-fixture-restored.txt`](../spikes/001-android-deja/evidence/walk005-fixture-restored.txt)
records the final stable selector setting, running device, and latest exact-hit passing report after
`warm-06`. No repair, source patch, or failed-test assertion change was made.

## Non-goals

No bounded repair, semantic retrieval, production CLI expansion, cross-platform claim, or use of the
historical samples as a cold/warm speedup claim.
