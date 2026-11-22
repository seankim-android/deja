# WalkCache TODO

The execution checklist for [`docs/PROJECT_PLAN.md`](docs/PROJECT_PLAN.md). Ticket scope,
dependencies, acceptance criteria, and evidence live in [`tickets/`](tickets/README.md).
Check an item only when its ticket is complete and its evidence is linked. All times are Pacific.

## Validated baseline

- [x] [WALK-001](tickets/WALK-001-android-feasibility-proof.md) — Validate Android cold exploration,
  independent verification, zero-model warm replay, stale-selector failure, and targeted source repair.
  Evidence: commit `eba52c3` and `spikes/001-android-walkcache/evidence/`.

## Current P0 queue — Android correctness first

- [x] [WALK-002](tickets/WALK-002-verified-maestro-runner.md) — Harden the verified Maestro runner adapter.
  Evidence: [`runner-unit-tests.txt`](spikes/001-android-walkcache/evidence/runner-unit-tests.txt) and
  [`runner-android-report.json`](spikes/001-android-walkcache/evidence/runner-android-report.json).
- [x] [WALK-003](tickets/WALK-003-self-contained-cold-explore.md) — Add self-contained `explore <goal>`.
  Evidence: [`explore-android-report.json`](spikes/001-android-walkcache/evidence/explore-android-report.json),
  [`explore-candidate.yaml`](spikes/001-android-walkcache/evidence/explore-candidate.yaml), and
  [`explore-rejection-preservation.json`](spikes/001-android-walkcache/evidence/explore-rejection-preservation.json).
- [x] [WALK-004](tickets/WALK-004-local-store-regression-tests.md) — Test cache identity, corruption,
  stale builds, path boundaries, and atomic replacement.
  Evidence: [`local-store-unit-tests.txt`](spikes/001-android-walkcache/evidence/local-store-unit-tests.txt),
  [`local-store-all-tests.txt`](spikes/001-android-walkcache/evidence/local-store-all-tests.txt), and the retained
  [`WALK-001 stale-build summary`](spikes/001-android-walkcache/evidence/run-summary.json).
- [x] [WALK-006](tickets/WALK-006-selector-failure-classification.md) — Repeat selector invalidation and
  distinguish selector failures from meaningful product regressions.
  Evidence: [`walk006-classification-matrix.md`](spikes/001-android-walkcache/evidence/walk006-classification-matrix.md),
  [`walk006-android-evidence.json`](spikes/001-android-walkcache/evidence/walk006-android-evidence.json), and
  [`walk006-all-tests.txt`](spikes/001-android-walkcache/evidence/walk006-all-tests.txt).
- [x] [WALK-005](tickets/WALK-005-comparable-cold-warm-metrics.md) — Capture comparable cold/warm metrics
  after the explore lifecycle and store tests are reliable.
  Evidence: [`walk005-comparison.md`](spikes/001-android-walkcache/evidence/walk005-comparison.md),
  [`walk005-comparison.json`](spikes/001-android-walkcache/evidence/walk005-comparison.json), and all eight
  linked raw reports in the ticket (two cold, six warm; every sample passed).
- [x] [WALK-007](tickets/WALK-007-bounded-repair-regression-guard.md) — Enforce bounded repair and prove
  a genuine product regression is never repaired into a pass.
  Evidence: [`walk007-repair-report.json`](spikes/001-android-walkcache/evidence/walk007-repair-report.json),
  [`walk007-repair.diff`](spikes/001-android-walkcache/evidence/walk007-repair.diff),
  [`walk007-negative-preservation.json`](spikes/001-android-walkcache/evidence/walk007-negative-preservation.json),
  [`walk007-negative-source.diff`](spikes/001-android-walkcache/evidence/walk007-negative-source.diff), and
  [`walk007-all-tests.txt`](spikes/001-android-walkcache/evidence/walk007-all-tests.txt).

## P0 — Headless product, then optional viewer

- [x] [WALK-008](tickets/WALK-008-production-cli-plugin-contracts.md) — Build the production CLI around
  store, runner, explorer, repair, and reporter contracts while retaining the validated spike.
  Evidence: [`CLI usage`](apps/cli/README.md),
  [`warm replay`](spikes/001-android-walkcache/evidence/walk008-warm.transcript.txt),
  [`stale repair`](spikes/001-android-walkcache/evidence/walk008-stale-repair.transcript.txt),
  [`regression guard`](spikes/001-android-walkcache/evidence/walk008-product-regression.transcript.txt),
  and [`80 tests / 500 assertions`](spikes/001-android-walkcache/evidence/walk008-all-tests.txt).
- [x] [WALK-009](tickets/WALK-009-local-run-viewer.md) — Replace the Vite starter with an optional local
  run/artifact viewer only after the CLI is reliable.
  Evidence: [`viewer guide`](viewer/README.md),
  [`warm screenshot`](spikes/001-android-walkcache/evidence/walk009-warm.png),
  [`16 focused tests`](spikes/001-android-walkcache/evidence/walk009-ui-test-artifacts.txt),
  [`no-viewer warm replay`](spikes/001-android-walkcache/evidence/walk009-headless-no-viewer.txt),
  and [`96 tests / 569 assertions`](spikes/001-android-walkcache/evidence/walk009-all-tests.txt).

## P0 — Additional platform evidence (cut before weakening Android)

- [x] [WALK-010](tickets/WALK-010-verified-web-proof.md) — Run the journey in Chromium through the web
  adapter, or document the concrete blocker and make no support claim.
  Evidence: [`support matrix`](spikes/002-web-walkcache/README.md),
  [`12-assertion warm replay / 0 model calls`](spikes/002-web-walkcache/evidence/walk010-warm.txt),
  [`build invalidation`](spikes/002-web-walkcache/evidence/walk010-build-invalidation.txt),
  [`genuine backend regression`](spikes/002-web-walkcache/evidence/walk010-product-regression.txt),
  [`Android compatibility replay`](spikes/001-android-walkcache/evidence/walk010-android-warm.txt), and
  [`101 tests / 599 assertions`](spikes/002-web-walkcache/evidence/walk010-all-tests.txt).
  Cold Web exploration and Web repair are explicitly unsupported, not silently substituted.
- [x] [WALK-011](tickets/WALK-011-verified-ios-proof.md) — Prove reviewed native iOS YAML, independent
  `remember`, zero-model warm replay, build invalidation, and genuine product-regression handling on
  iPhone 17 / iOS 27.0 through the dedicated adapter/simulator. Evidence:
  [`support boundary`](spikes/003-ios-walkcache/README.md),
  [`warm replay`](spikes/003-ios-walkcache/evidence/walk011-warm.txt),
  [`regression guard`](spikes/003-ios-walkcache/evidence/walk011-product-regression.txt),
  and [`105 tests / 621 assertions`](spikes/003-ios-walkcache/evidence/walk011-all-tests.txt).
  Cold exploration and repair are explicitly unsupported. The post-change Android replay was attempted
  but blocked before boot by emulator disk capacity; no device compatibility pass is claimed.

## P0 — OSS release and submission

- [x] [WALK-012](tickets/WALK-012-oss-docs-metadata.md) — Add accurate quick start, architecture,
  security/limitations, Apache-2.0 license, and package metadata.
  Evidence: [`README`](README.md), [`ARCHITECTURE`](docs/ARCHITECTURE.md), [`LICENSE`](LICENSE),
  [`package.json`](package.json), and the author-executed
  [`README quick-start runtime review`](spikes/001-android-walkcache/evidence/walk012-readme-review.txt),
  and [`independent clean-context PASS review`](spikes/001-android-walkcache/evidence/walk012-independent-review.md).
  The author fixed all broken ticket-evidence links, aligned executable-launch exit documentation
  with the reproduced exit-1 runner result, removed dead starter SVGs, and the reviewer rechecked them.
  Checks: 106 tests / 626 assertions, lint, TypeScript/Vite build, and `git diff --check`.
- [x] [WALK-013](tickets/WALK-013-portable-tests-clean-ci.md) — Pass portable tests and clean-checkout CI.
  Evidence: [`clean clone`](spikes/001-android-walkcache/evidence/walk013-clean-checkout.txt),
  [`standalone Android YAML`](spikes/001-android-walkcache/evidence/walk013-standalone-android.txt),
  [`artifact validation`](spikes/001-android-walkcache/evidence/walk013-artifact-validation.txt),
  [`CI guide`](docs/CI.md), and
  [passing GitHub Actions run 45060550371](https://github.com/seankim-android/kimi-hackathon/actions/runs/45060550371)
  on `main` at `b827311` (every portable step green in 15 seconds).
- [x] [WALK-014](tickets/WALK-014-demo-assets-rehearsal.md) — Prepare deterministic demo assets and
  complete two evidence-checked rehearsals before the 6:30 AM milestone gate.
  Evidence: [`58-second MP4`](demo/walkcache-demo.mp4), [`script`](demo/SCRIPT.md),
  [`two passing rehearsals`](demo/rehearsal/two-rehearsals.json), and
  [`rejected-take/review record`](demo/rehearsal/REVIEW.md). Uploaded in the authorized Slack thread
  as `F0CKVLY06F7` (superseding `F0CKUL64YBD`). AI visual review plus all
  1,740 encoded-frame/source comparisons passed; this is not claimed as human sign-off. External submission remains open; the
  milestone/deadline passed before this late fallback was completed.
- [x] [WALK-020](tickets/WALK-020-live-device-demo.md) — Replace static evidence with actual uncut
  Chromium/Android before/after recordings and a reviewed, timestamp-mapped ≤60-second live cut.
  Accepted raw runs: Chromium `walk020-20261122T195607-94392` and Android
  `walk020-android-checkout-06`; fresh cold/warm plus separately disclosed valid repair fixture.
  [`Final live MP4`](demo/walkcache-live-demo.mp4): 59.733333 seconds, 1,792 decoded frames,
  SHA-256 `d834bda3b156137df2693d11297855180e8e5c681fc870417fa05a0647aec945`.
  [`Android review`](demo/live-evidence/android/REVIEW.md),
  [`Chromium review`](demo/live-evidence/chromium/REVIEW.md), and
  [`final automated/AI review`](demo/live-evidence/FINAL_REVIEW.md) pass; raw decode/hash checks,
  seven segment maps and secret/private-path scans pass. Human sign-off is not claimed.
  Uploaded to the authorized Slack thread as `F0CL4RMSE4C`; the
  [`delivery receipt`](demo/live-evidence/slack-upload.json) matches the SHA-256 and the
  [`final delivery gate`](demo/live-evidence/final-delivery-check.json) passes raw checks and scans
  of 74 curated text files. No social post or official submission is included.
- [ ] [WALK-015](tickets/WALK-015-video-submission.md) — Record/review a ≤60-second video, publish the
  required authorized social post, and confirm official submission by 9:00 AM. Keep 9:00–10:00 AM for
  upload/submission failures; the recorded deadline is 10:00 AM on November 22, 2026.

## P1 — Deferred until all relevant P0 gates pass

- [x] [WALK-016](tickets/WALK-016-semantic-goal-retrieval.md) — Exact-match-first semantic retrieval.
  Evidence: [acceptance matrix/limitations](spikes/001-android-walkcache/evidence/walk016-semantic-retrieval.md),
  [47 focused tests / 271 assertions](packages/core/test/semantic.test.ts), and the
  [138 tests / 774 assertions](spikes/001-android-walkcache/evidence/walk016-all-tests.txt).
  The opt-in `local-lexical-v1` proof makes no generalized semantic-correctness claim.
- [x] [WALK-017](tickets/WALK-017-route-graph-screen-fingerprints.md) — Route graph and screen fingerprints.
  Evidence: [`four-node Android graph`](spikes/001-android-walkcache/evidence/walk017-route-graph.json),
  [`route-graph guide`](docs/ROUTE_GRAPH.md), and 25 focused tests (131 full-suite tests / 699 assertions).
  Derived solely from existing curated WALK-020 cold/warm evidence; no new device run or route-based
  cache behavior is claimed. Unknown/changed/ambiguous states fail closed to existing orchestration.
- [ ] [WALK-018](tickets/WALK-018-reusable-login-subflow.md) — Reuse a verified login subflow.
- [x] [WALK-019](tickets/WALK-019-store-plugin-conformance.md) — Second local Bun SQLite store,
  transactional metadata replacement, actual YAML persistence, and one unchanged contract suite against
  both stores. Evidence: [`44 focused tests / 153 assertions and both zero-model orchestrations`](spikes/001-android-walkcache/evidence/walk019-store-conformance.txt),
  [`132 full tests / 719 assertions`](spikes/001-android-walkcache/evidence/walk019-all-tests.txt),
  [`lint`](spikes/001-android-walkcache/evidence/walk019-lint.txt), and
  [`build`](spikes/001-android-walkcache/evidence/walk019-build.txt). Setup, atomicity, single-writer,
  and durability boundaries are documented in [`packages/store-sqlite/README.md`](packages/store-sqlite/README.md).

## Standing cut and safety rules

- Do not turn historical WALK-001/WALK-005 spike timings into a current-CLI measured speedup claim.
  Current proof establishes zero-model replay; nested Maestro runner ranges overlap, and any future
  performance comparison must use equivalent complete lifecycles and new raw evidence.
- Never save an unverified candidate, weaken a product assertion, auto-commit a repair, or hide a real
  product regression behind a selector/source patch.
- Keep generated `.walkcache/`, build, temporary, credential, and personal-data artifacts out of Git;
  commit only curated secret-free evidence.
- No hosted service, unrestricted source edits, autonomous PR creation, broad app-wide selector
  generation, or unsupported cross-platform claims in the hackathon MVP.
