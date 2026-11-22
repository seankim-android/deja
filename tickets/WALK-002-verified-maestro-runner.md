---
id: WALK-002
title: Harden the verified Maestro runner adapter
milestone: M2A
priority: P0
status: ready
depends_on: [WALK-001]
---

# WALK-002 — Harden the verified Maestro runner adapter

## Outcome

Turn the spike's working `runMaestro` function into a reusable adapter without changing the green
Android journey or making machine-specific paths part of the product contract.

## Checklist

- [ ] Accept executable, device, platform, flow, timeout, SDK, Java, and artifact configuration.
- [ ] Preserve stdout/stderr, start/end timestamps, monotonic duration, exit code, and pass/fail.
- [ ] Enforce a timeout and terminate only the spawned process tree; never stop another emulator or run.
- [ ] Return structured launch, timeout, execution, and output-artifact errors.
- [ ] Resolve flows through a workspace boundary and reject path escapes.
- [ ] Keep generated output ignored and prevent secret-bearing environment dumps in reports.
- [ ] Add focused tests for success, nonzero exit, timeout, missing executable, and invalid flow path.
- [ ] Re-run the curated Android flow against the same emulator as an integration check.

## Acceptance evidence

Link unit-test output plus one passing Android report containing `cacheStatus`, `modelCalls`,
`durationMs`, and `buildFingerprint`. Document exact configuration and the failure/timeout commands.

## Non-goals

No exploration, source edits, semantic failure inference, dashboard, or platform abstraction rewrite.
Selector/product failure classification belongs to WALK-006.

