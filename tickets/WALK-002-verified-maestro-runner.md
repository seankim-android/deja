---
id: WALK-002
title: Harden the verified Maestro runner adapter
milestone: M2A
priority: P0
status: done
depends_on: [WALK-001]
---

# WALK-002 — Harden the verified Maestro runner adapter

## Outcome

Turn the spike's working `runMaestro` function into a reusable adapter without changing the green
Android journey or making machine-specific paths part of the product contract.

## Checklist

- [x] Accept executable, device, platform, flow, timeout, SDK, Java, and artifact configuration.
- [x] Preserve stdout/stderr, start/end timestamps, monotonic duration, exit code, and pass/fail.
- [x] Enforce a timeout and terminate only the spawned process tree; never stop another emulator or run.
- [x] Return structured launch, timeout, execution, and output-artifact errors.
- [x] Resolve flows through a workspace boundary and reject path escapes.
- [x] Keep generated output ignored and prevent secret-bearing environment dumps in reports.
- [x] Add focused tests for success, nonzero exit, timeout, missing executable, and invalid flow path.
- [x] Re-run the curated Android flow against the same emulator as an integration check.

## Acceptance evidence

Link unit-test output plus one passing Android report containing `cacheStatus`, `modelCalls`,
`durationMs`, and `buildFingerprint`. Document exact configuration and the failure/timeout commands.

## Reproducible evidence

- Focused test output: [`runner-unit-tests.txt`](../spikes/001-android-walkcache/evidence/runner-unit-tests.txt).
  The final run passed 6 tests and 53 assertions in 1.54s, including output-artifact failure coverage
  and a timeout test that observed an independent bystander surviving while the spawned tree died.
- Passing Android report: [`runner-android-report.json`](../spikes/001-android-walkcache/evidence/runner-android-report.json).
  It records `cacheStatus: "hit"`, `modelCalls: 0`, `passed: true`, `exitCode: 0`,
  `durationMs: 25778`, `monotonicDurationMs: 25779`, and APK SHA-256 `buildFingerprint:
  "4746296fa8aa84ae85761915f651b1488c38ef8f5f4e89af0e9ad19784cfcb05"`.
  Execution started at `2026-11-22T04:49:00.117Z` and ended at `2026-11-22T04:49:25.895Z`.
  This is another approximately 25-second Maestro execution, not evidence of a runtime speedup.

### Exact Android configuration and command

- Workspace: `/Users/seankim/.openclaw/workspace/kimi-hackathon/spikes/001-android-walkcache`.
- Maestro 2.12.0: `/Users/seankim/.maestro/bin/maestro` (also overridable with
  `WALKCACHE_MAESTRO`).
- Device: `emulator-5554`, AVD `walkcache_api_34`; its `adb` state was still `device` after the run.
- Platform: Android; flow: `.walkcache/flows/checkout/android.yaml`.
- Timeout: `120000` ms; no timeout occurred.
- Android SDK: `/opt/homebrew/share/android-commandlinetools`.
- Java: `/opt/homebrew/opt/openjdk@21`, OpenJDK `21.0.13.0.3`.
- Generated artifacts: `.walkcache/reports/run-1795322940117/{stdout,stderr,output}.log`.
  These logs and `.tmp/` remain ignored; only the secret-free structured report is curated.

From the repository root:

```sh
cd spikes/001-android-walkcache
. ./scripts/env.sh
WALKCACHE_DEVICE=emulator-5554 WALKCACHE_TIMEOUT_MS=120000 \
  bun scripts/walkcache.ts run checkout
cp .walkcache/reports/latest.json evidence/runner-android-report.json
```

### Unit, failure, and timeout commands

From the repository root, reproduce the complete recorded test output:

```sh
bun test packages/maestro/test/maestro-runner.test.ts --timeout=10000 --check \
  2>&1 | tee spikes/001-android-walkcache/evidence/runner-unit-tests.txt
```

Run individual failure paths with these filters (the tests pass by asserting the structured
failure; they do not require a real failing Android app):

```sh
bun test packages/maestro/test/maestro-runner.test.ts --timeout=10000 --check -t 'nonzero exit'
bun test packages/maestro/test/maestro-runner.test.ts --timeout=10000 --check -t 'enforces the timeout'
bun test packages/maestro/test/maestro-runner.test.ts --timeout=10000 --check -t 'missing executable'
bun test packages/maestro/test/maestro-runner.test.ts --timeout=10000 --check -t 'escapes the workspace'
bun test packages/maestro/test/maestro-runner.test.ts --timeout=10000 --check -t 'artifact error'
```

`bun run build` and `bun run lint` also passed after integration. No emulator or unrelated run was
stopped by the implementation or verification.

## Non-goals

No exploration, source edits, semantic failure inference, dashboard, or platform abstraction rewrite.
Selector/product failure classification belongs to WALK-006.

