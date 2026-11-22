# WalkCache ticket backlog

These are repo-local tickets for **WalkCache Hackathon MVP**. They replace the blocked External Linear
setup as the working backlog; `WALK-` identifiers are stable local identifiers, not Linear links.

## Workflow

- Work in priority/dependency order. Preserve the validated Android spike before expanding scope.
- Keep one ticket's implementation, tests, evidence, status, and `TODO.md` checkbox together.
- A ticket is `done` only when every acceptance criterion has linked, reproducible evidence.
- Record exact commands and measured values. Use `unknown` when instrumentation is unavailable;
  never turn missing measurements into claims.
- Do not create Linear objects, publish social posts, or submit the hackathon merely by completing
  a repo ticket. Those external actions require their own authorized execution.
- Cut P1 and unverified platform work before weakening cold/warm behavior, meaningful assertions,
  zero-model replay, or regression safety.

Statuses: `in_progress`, `done`, `ready` (no incomplete dependencies), `queued` (later P0 work), `deferred` (P1).

## P0 — Android core

| Ticket | Status | Depends on | Outcome |
| --- | --- | --- | --- |
| [WALK-001](WALK-001-android-feasibility-proof.md) | done | — | Validated native Android cold/warm/repair spike |
| [WALK-002](WALK-002-verified-maestro-runner.md) | done | 001 | Portable, bounded runner with structured results |
| [WALK-003](WALK-003-self-contained-cold-explore.md) | done | 002 | One-command cold exploration and verified persistence |
| [WALK-004](WALK-004-local-store-regression-tests.md) | done | 002 | Cache identity, corruption, and atomic-write tests |
| [WALK-005](WALK-005-comparable-cold-warm-metrics.md) | done | 003, 004 | Honest end-to-end cold/warm measurements |
| [WALK-006](WALK-006-selector-failure-classification.md) | done | 002 | Repeatable invalidation and regression classification |
| [WALK-007](WALK-007-bounded-repair-regression-guard.md) | done | 006 | Safe one-attempt repair and negative regression proof |

## P0 — Product and verified platforms

| Ticket | Status | Depends on | Outcome |
| --- | --- | --- | --- |
| [WALK-008](WALK-008-production-cli-plugin-contracts.md) | done | 003–007 | Headless CLI around plugin interfaces |
| [WALK-009](WALK-009-local-run-viewer.md) | done | 008 | Optional local dashboard for runs and artifacts |
| [WALK-010](WALK-010-verified-web-proof.md) | done | 005, 007, 008 | Verified Chromium replay; cold exploration/repair explicitly unsupported |
| [WALK-011](WALK-011-verified-ios-proof.md) | ready | 005, 007, 008 | Real iOS simulator evidence or named blocker |

## P0 — Release

| Ticket | Status | Depends on | Outcome |
| --- | --- | --- | --- |
| [WALK-012](WALK-012-oss-docs-metadata.md) | ready | 008 | Accurate quick start, architecture, license, limitations |
| [WALK-013](WALK-013-portable-tests-clean-ci.md) | queued | 012 | Portable tests and clean-checkout CI evidence |
| [WALK-014](WALK-014-demo-assets-rehearsal.md) | queued | 005, 007, 008, 013 | Reproducible demo and two verified rehearsals |
| [WALK-015](WALK-015-video-submission.md) | queued | 014 | Reviewed ≤60-second video and confirmed submission |

## P1 — Only after relevant P0 gates pass

| Ticket | Status | Outcome |
| --- | --- | --- |
| [WALK-016](WALK-016-semantic-goal-retrieval.md) | deferred | Exact-match-first semantic retrieval |
| [WALK-017](WALK-017-route-graph-screen-fingerprints.md) | deferred | Minimal route graph and screen-state fingerprints |
| [WALK-018](WALK-018-reusable-login-subflow.md) | deferred | Verified login subflow reused by another goal |
| [WALK-019](WALK-019-store-plugin-conformance.md) | deferred | Second store and conformance tests |

