# WalkCache TODO

The execution checklist for [`docs/PROJECT_PLAN.md`](docs/PROJECT_PLAN.md). Ticket scope,
dependencies, acceptance criteria, and evidence live in [`tickets/`](tickets/README.md).
Check an item only when its ticket is complete and its evidence is linked. All times are Pacific.

## Validated baseline

- [x] [WALK-001](tickets/WALK-001-android-feasibility-proof.md) — Validate Android cold exploration,
  independent verification, zero-model warm replay, stale-selector failure, and targeted source repair.
  Evidence: commit `eba52c3` and `spikes/001-android-walkcache/evidence/`.

## Current P0 queue — Android correctness first

- [ ] [WALK-002](tickets/WALK-002-verified-maestro-runner.md) — Harden the verified Maestro runner adapter.
- [ ] [WALK-003](tickets/WALK-003-self-contained-cold-explore.md) — Add self-contained `explore <goal>`.
- [ ] [WALK-004](tickets/WALK-004-local-store-regression-tests.md) — Test cache identity, corruption,
  stale builds, path boundaries, and atomic replacement.
- [ ] [WALK-006](tickets/WALK-006-selector-failure-classification.md) — Repeat selector invalidation and
  distinguish selector failures from meaningful product regressions.
- [ ] [WALK-005](tickets/WALK-005-comparable-cold-warm-metrics.md) — Capture comparable cold/warm metrics
  after the explore lifecycle and store tests are reliable.
- [ ] [WALK-007](tickets/WALK-007-bounded-repair-regression-guard.md) — Enforce bounded repair and prove
  a genuine product regression is never repaired into a pass.

## P0 — Headless product, then optional viewer

- [ ] [WALK-008](tickets/WALK-008-production-cli-plugin-contracts.md) — Build the production CLI around
  store, runner, explorer, repair, and reporter contracts while retaining the validated spike.
- [ ] [WALK-009](tickets/WALK-009-local-run-viewer.md) — Replace the Vite starter with an optional local
  run/artifact viewer only after the CLI is reliable.

## P0 — Additional platform evidence (cut before weakening Android)

- [ ] [WALK-010](tickets/WALK-010-verified-web-proof.md) — Run the journey in Chromium through the web
  adapter, or document the concrete blocker and make no support claim.
- [ ] [WALK-011](tickets/WALK-011-verified-ios-proof.md) — Run the journey on an iOS simulator through
  the iOS adapter, or document the concrete blocker and make no support claim.

## P0 — OSS release and submission

- [ ] [WALK-012](tickets/WALK-012-oss-docs-metadata.md) — Add accurate quick start, architecture,
  security/limitations, Apache-2.0 license, and package metadata.
- [ ] [WALK-013](tickets/WALK-013-portable-tests-clean-ci.md) — Pass portable tests and clean-checkout CI.
- [ ] [WALK-014](tickets/WALK-014-demo-assets-rehearsal.md) — Prepare deterministic demo assets and
  complete two evidence-checked rehearsals before the 6:30 AM milestone gate.
- [ ] [WALK-015](tickets/WALK-015-video-submission.md) — Record/review a ≤60-second video, publish the
  required authorized social post, and confirm official submission by 9:00 AM. Keep 9:00–10:00 AM for
  upload/submission failures; the recorded deadline is 10:00 AM on November 22, 2026.

## P1 — Deferred until all relevant P0 gates pass

- [ ] [WALK-016](tickets/WALK-016-semantic-goal-retrieval.md) — Exact-match-first semantic retrieval.
- [ ] [WALK-017](tickets/WALK-017-route-graph-screen-fingerprints.md) — Route graph and screen fingerprints.
- [ ] [WALK-018](tickets/WALK-018-reusable-login-subflow.md) — Reuse a verified login subflow.
- [ ] [WALK-019](tickets/WALK-019-store-plugin-conformance.md) — Second store and plugin conformance tests.

## Standing cut and safety rules

- Do not claim cold/warm speedup until WALK-005 measures it; the current proof establishes zero-model
  replay, with Maestro executions around 25 seconds in both verified paths.
- Never save an unverified candidate, weaken a product assertion, auto-commit a repair, or hide a real
  product regression behind a selector/source patch.
- Keep generated `.walkcache/`, build, temporary, credential, and personal-data artifacts out of Git;
  commit only curated secret-free evidence.
- No hosted service, unrestricted source edits, autonomous PR creation, broad app-wide selector
  generation, or unsupported cross-platform claims in the hackathon MVP.
