---
id: WALK-018
title: Reuse a verified login subflow in another checkout goal
milestone: P1 composition
priority: P1
status: done
depends_on: [WALK-017]
---

# WALK-018 — Reuse a verified login subflow in another checkout goal

## Outcome

Compose a new journey from an independently verified login segment while preserving full-journey
assertions and cache safety.

## Checklist

- [x] Define typed subflow inputs/outputs, app/build identity, preconditions, and postconditions.
- [x] Save reusable segments only after independent verification.
- [x] Compose login with a materially different checkout goal without duplicating login steps.
- [x] Verify the complete composed journey from clean state before saving it as a successful walk.
- [x] Ensure failed composition cannot overwrite either the login segment or previous goal memory.
- [x] Report reused versus newly explored steps and any model calls separately.

## Acceptance evidence

- [Typed contract and orchestration guide](../docs/SUBFLOW_COMPOSITION.md), with the closed
  [core schema](../packages/core/src/subflow.ts), independent
  [Maestro composer](../packages/maestro/src/subflow-composition.ts), and checksummed
  [local segment store](../packages/store-local/src/verified-subflow-store.ts). Exact goal-cache
  lookup/order/orchestration is unchanged; composition is explicit, not route planning.
- [Login contract](../spikes/001-android-walkcache/evidence/walk018/login-contract.json) and
  [independently verified login YAML](../spikes/001-android-walkcache/evidence/walk018/segments/login-v1/android-c415a62dae8b1346213ac1f92c5201d2a8c17e01d98c84e6ab979f4d785f3266.yaml):
  required text/credential input descriptors (no metadata values), Android app/build identity,
  clean-launch and pre-action `screen_login` preconditions, catalog output/postcondition, 11 steps
  and 5 assertions.
- [Composed YAML](../spikes/001-android-walkcache/evidence/walk018/composed/sign-in-and-check-out-mochi-sticker-pack/candidate.yaml)
  for **Sign in and check out Mochi sticker pack**, plus its
  [11-step checkout continuation](../spikes/001-android-walkcache/evidence/walk018/continuation/checkout.yaml).
  One `runFlow` reference replaces all login commands and fixture values. Reused/new steps are **11 /
  11**; reused/new assertions are **5 / 9**; the full journey has 22 steps and all 14 original
  assertions. New exploration, composition, segment verification, and full verification model calls
  are separately reported as **0 / 0 / 0 / 0**.
- [Clean-state verification and failure-preservation report](../spikes/001-android-walkcache/evidence/walk018/walk018-verification.json):
  independently execute login; verify the complete composed candidate before persistence; save it;
  replay stored YAML through unchanged exact `WalkCache.run` with `hit`/`passed`, 14 assertions,
  zero model calls and zero explorer constructions; then inject a missing checkout control and prove
  login index/YAML, previous goal flow and complete last-known-good memory remain byte-for-byte
  unchanged. Raw [clean](../spikes/001-android-walkcache/evidence/walk018/walk018-clean-state-composition.log),
  [exact-replay](../spikes/001-android-walkcache/evidence/walk018/walk018-exact-replay.log), and
  [failed](../spikes/001-android-walkcache/evidence/walk018/walk018-failed-composition.log) logs are
  linked by the report. Final confirmation and exact WC-1001 item assertions are selected from
  actual runner observations on both clean composition and replay. Additional adversarial evidence
  rejects a wrong WC-9999 final text as a product assertion and rejects a green runner containing a
  late second clean launch through bounded observations; both preserve goal memory, prior flow,
  segment index, and segment YAML byte-for-byte. Raw [wrong-text](../spikes/001-android-walkcache/evidence/walk018/walk018-wrong-final-text.log)
  and [late-launch](../spikes/001-android-walkcache/evidence/walk018/walk018-late-second-clear-launch.log)
  logs record `finalProductAssertionPassed: false` and two launches/final `login`, respectively.
- [42 focused tests / 237 assertions](../spikes/001-android-walkcache/evidence/walk018/walk018-focused-tests.txt)
  cover schema/identity, rejected independent verification, contract-accounting mismatches, missing
  observation traces, containment-safe same-directory and nested references plus normalized,
  absolute and parent escapes, reference cycles, exact runner/storage integration, runner-failure
  preservation, and filesystem failure **after passing verification**. Separate segment-store cases
  prove index/flow replacement failures preserve the previous segment byte-for-byte.
  [Full portable suite: 215 tests / 1,083 assertions](../spikes/001-android-walkcache/evidence/walk018/walk018-all-tests.txt);
  [lint/build](../spikes/001-android-walkcache/evidence/walk018/walk018-lint-build.txt) passed.
- Independently runnable deriver/verifier: `bun scripts/derive-walk018-composition.ts --check` and
  `bun scripts/verify-walk018-composition.ts --check`; their fresh process executions passed in
  [reproduction evidence](../spikes/001-android-walkcache/evidence/walk018/walk018-reproduction-check.txt).
  `git diff --check` passed, and the release-artifact validator was run after committing on the
  clean checkout; see the handoff for its commit-bound result.

## Review-resolution regressions

The following adversarial cases were reproduced on the original local commit `9694e96` before the
hardening changes and are now executable in every verifier `--check` run:

| Case | Original result | Current required result |
| --- | --- | --- |
| Final item changed to `Order #WC-9999 · Wrong item` | Exit 0, 14 assertions, `finalProductAssertionPassed: true` | Rejected `product_assertion`, `finalProductAssertionPassed: false`, no goal persistence |
| Second clean launch appended after `screen_catalog`, with declared step count adjusted to 12 | `status: verified`, `passed: true`, `persisted: true` | Runner still passes with two launches and final `login`, but observation validation returns `rejected`, no segment write |
| `login.yaml` versus `../../../outside.yaml` references | Safe same-directory reference rejected; escape accepted | Same-directory and normalized in-root references accepted; parent, normalized, absolute, backslash, and containing-directory escapes rejected |

For both execution regressions the report compares before/after SHA-256 and byte lengths for goal
memory, segment index, segment YAML, and previous goal YAML; each pair is equal. The accepted
composition still has one clean launch, 22 steps, 14 assertions, both exact final observations, and
an exact zero-model cache replay.

**Exact provenance:** existing accepted WALK-020 `walk020-android-checkout-06` cold evidence only:
`cold-saved-checkout.yaml` SHA-256 `e95532ce04a4e125d3bd56e1b9d5d367b31dd67eed4441c56271ca52f3c3d0a1`,
`cold-stored-memory.json` passing at `2026-11-22T20:27:33.668Z`, and `cold-run-report.json` with 14
assertions for app `dev.walkcache.checkout` / build
`6aec5fea876938db14007ba3dbcb91f698832fe855886abd9dea1e7b28861759`. The prior cold report's
13 whole-flow model calls remain there and are not assigned to the split. WALK-018 used deterministic
derivation and executable process fixtures only: no emulator/simulator, external service, new model
exploration, or new real-device run.

Acceptance is limited to this narrow Android login-plus-checkout composition. No general route
planning, subflow discovery/parameter substitution, multi-app/Web/iOS composition, current device
compatibility, or performance result is claimed. WALK-017 remains diagnostic-only.

