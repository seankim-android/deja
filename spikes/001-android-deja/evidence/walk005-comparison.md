# WALK-005 — Comparable cold/warm Android metrics

## Verdict

All controlled samples passed the unchanged meaningful product assertions.
Warm replay avoided model calls in every warm sample (0, 0, 0, 0, 0, 0), while cold
exploration used actual OpenClaw instrumentation (13, 13 assistant turns).
**Model avoidance is not the same as Maestro execution speed.** The median complete-command wall-time
delta is 71,628.5 ms cold minus warm; the median nested runner-time delta is
395 ms. These are observed sample differences, not a universal speedup claim.

## Exact boundaries and equivalent conditions

Both modes used the same APK SHA-256 `47ad33ba3cab8d4c1c268b1522e5ee7fd2d799fc3da6fdc884f99a40b7a166aa`, goal
`checkout`, app `dev.walkcache.checkout`,
device `emulator-5554`, stable selector fixture, clearState app launch,
and exact `Order #WC-1001` outcome assertion. Emulator boot, build, installation, and fixture selection were
completed before the series and are excluded from both command-wall timers.

- **Cold start:** Immediately before spawning bun scripts/walkcache.ts explore checkout --config explore.config.json.
- **Cold end:** After that CLI process exits, including its final report write.
- **Cold includes:** Bun/CLI startup, configuration loading, APK fingerprinting, and attempt setup. The exploration-phase adb pm clear device setup. The complete OpenClaw/Kimi exploration, Maestro MCP activity, and candidate authoring. The verification-phase adb pm clear and independent WALK-002 clearState Maestro execution. Atomic verified-flow persistence and report writing.
- **Warm start:** Immediately before spawning bun scripts/walkcache.ts run checkout.
- **Warm end:** After that CLI process exits, including validation-history persistence and its final report write.
- **Warm includes:** Bun/CLI startup, APK fingerprinting, exact cache lookup, and stored-flow loading. The WALK-002 Maestro process and its flow-level clearState app launch. Validation-history persistence and report writing.

The outer command-wall timer is directly comparable. Internal timers answer different questions: cold
lifecycle includes exploration, verification, and persistence; warm's existing report timer is runner-only.

## Side-by-side aggregates

Median with full observed minimum–maximum range; no best-case selection.

| Metric | Cold (2 samples) | Warm (6 samples) |
| --- | --- | --- |
| Complete command wall | 96,757.5 ms (range 96,518 ms–96,997 ms) | 25,129 ms (range 24,309 ms–25,864 ms) |
| Nested runner | 25,505.5 ms (range 25,215 ms–25,796 ms) | 25,110.5 ms (range 24,288 ms–25,845 ms) |
| Exploration | 71,033.5 ms (range 70,495 ms–71,572 ms) | unknown |
| Independent verification wrapper | 25,507 ms (range 25,217 ms–25,797 ms) | unknown |
| MCP calls | 11.5 (range 11–12) | 0 (range 0–0) |
| Model calls / assistant turns | 13 (range 13–13) | 0 (range 0–0) |

Mean command wall: cold 96,757.5 ms, warm 25,080.167 ms.
Cold wall variability: 479 ms; warm wall variability:
1,555 ms. Cold exploration variability:
1,077 ms; cold verification-wrapper variability:
580 ms; nested runner variability: cold
581 ms, warm 1,557 ms.

## Raw samples

| Sample | Pair | Mode | Command wall | Nested runner | MCP calls | Model calls | Cache | Verification |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| cold-01 | pair-1 | cold | 96,997 ms | 25,215 ms | 11 | 13 | recorded | PASS |
| cold-02 | pair-2 | cold | 96,518 ms | 25,796 ms | 12 | 13 | recorded | PASS |
| warm-01 | pair-1 | warm | 24,461 ms | 24,442 ms | 0 | 0 | hit | PASS |
| warm-02 | pair-1 | warm | 24,309 ms | 24,288 ms | 0 | 0 | hit | PASS |
| warm-03 | pair-1 | warm | 25,589 ms | 25,568 ms | 0 | 0 | hit | PASS |
| warm-04 | pair-2 | warm | 25,112 ms | 25,094 ms | 0 | 0 | hit | PASS |
| warm-05 | pair-2 | warm | 25,864 ms | 25,845 ms | 0 | 0 | hit | PASS |
| warm-06 | pair-2 | warm | 25,146 ms | 25,127 ms | 0 | 0 | hit | PASS |

Cold MCP counts are observed `tools/call` requests. Cold model counts are OpenClaw
`assistantTurns` (completed provider round trips), never inferred from MCP. Warm MCP/model counts are zero
because the direct `run` path spawns neither Maestro MCP nor OpenClaw; this is separate from runner duration.

## Historical evidence (not comparable)

The 24.824 s and 24.510 s WALK-001 runs remain historical-only in
`spikes/001-android-walkcache/evidence/run-summary.json`. They are excluded from every aggregate above.
