---
id: WALK-001
title: Validate the Android WalkCache feasibility proof
milestone: M0–M4 spike
priority: P0
status: done
depends_on: []
---

# WALK-001 — Validate the Android WalkCache feasibility proof

## Outcome

Prove the riskiest product assumption on Android before building production abstractions: Kimi can
author a journey through Maestro, WalkCache can replay it without a model, and a stale selector is
not mistaken for a passing product test.

## Verified evidence

- [x] Boot a dedicated Android API 34 emulator and discover it with Maestro 2.12.0.
- [x] Build and install a dependency-free native Java login → add item → checkout fixture.
- [x] Pass the hand-written 15-command baseline without an agent.
- [x] Let an isolated Kimi explorer, denied app source and the reference flow, use 21 Maestro MCP
  calls to author a 21-command candidate with meaningful assertions.
- [x] Independently verify the candidate in 25.101 seconds before saving it.
- [x] Replay a warm hit in 24.824 seconds with `cacheStatus: "hit"` and `modelCalls: 0`.
- [x] Make `checkout_button` stale and fail only at that selector without a false pass or model call.
- [x] Patch only two affected source files, show the diff, rebuild, and verify in 25.784 seconds.
- [x] Detect the changed APK fingerprint as a cache miss with exit code 2.
- [x] Pass the repaired warm replay in 24.510 seconds with zero model calls.
- [x] Pass root lint, production build, JSON validation, and `git diff --check`.

## Evidence

- `spikes/001-android-deja/README.md`
- `spikes/001-android-deja/evidence/run-summary.json`
- `spikes/001-android-deja/evidence/agent-cold-checkout.yaml`
- `spikes/001-android-deja/evidence/checkout-selector-repair.diff`
- Commit `eba52c3` (`feat: validate Android WalkCache POC`)

## Measurement boundary

This proves model avoidance, not a cold-versus-warm wall-clock speedup. Total cold duration and exact
model-call count are unknown. WALK-005 owns comparable measurements. Generalized classification and
the deliberate product-regression negative test remain in WALK-006 and WALK-007.

