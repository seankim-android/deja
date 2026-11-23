---
id: WALK-017
title: Add a minimal route graph and screen-state fingerprints
milestone: P1 navigation memory
priority: P1
status: done
depends_on: [WALK-008]
---

# WALK-017 — Add a minimal route graph and screen-state fingerprints

## Outcome

Record verified navigation structure to improve diagnosis and future composition without turning an
incomplete graph into a correctness claim.

## Checklist

- [x] Define bounded screen fingerprints from stable non-secret observations.
- [x] Record nodes, verified actions/transitions, app/build identity, and validation timestamps.
- [x] Store failed branches separately from successful routes.
- [x] Detect changed screens conservatively and fall back to existing verified replay/exploration.
- [x] Avoid raw credentials, personal data, screenshots, or unrestricted environment dumps.
- [x] Add deterministic fixtures for repeated, ambiguous, changed, and unknown screens.

## Acceptance evidence

- [Implementation and schema](../packages/core/src/route-graph.ts), with
  [22 deterministic schema/fingerprint/matching tests](../packages/core/test/route-graph.test.ts) and
  [3 curated-evidence tests](../packages/core/test/route-graph-evidence.test.ts). Focused result:
  **25 tests / 73 assertions passed**. Full portable result: **131 tests / 699 assertions passed**.
- [Small Android route graph](../spikes/001-android-deja/evidence/walk017-route-graph.json):
  four nodes and three verified transitions for `dev.walkcache.checkout`, build
  `6aec5fea876938db14007ba3dbcb91f698832fe855886abd9dea1e7b28861759`.
- [Route-graph guide](../docs/ROUTE_GRAPH.md) documents the closed bounds, non-secret observations,
  separate failed branches, conservative statuses/fallback, exact provenance, reproduction commands,
  and acceptance limits. Unknown, changed, and ambiguous observations always return `cacheHit: false`
  and the existing exact replay/exploration fallback; no graph result enters `WalkStore`.
- [Deterministic deriver](../scripts/derive-walk017-route-graph.ts) validates and rehashes the source
  evidence, runs schema validation, and byte-compares the curated graph:
  `bun scripts/derive-walk017-route-graph.ts --check` passed.
- **Exact Android provenance:** existing accepted WALK-020 run `walk020-android-checkout-06`, fresh
  cold/warm lifecycle only. Sources are byte-identical
  [`cold-saved-checkout.yaml`](../demo/live-evidence/android/cold-saved-checkout.yaml) and
  [`warm-saved-checkout.yaml`](../demo/live-evidence/android/warm-saved-checkout.yaml) (flow SHA-256
  `e95532ce04a4e125d3bd56e1b9d5d367b31dd67eed4441c56271ca52f3c3d0a1`),
  [`cold-stored-memory.json`](../demo/live-evidence/android/cold-stored-memory.json),
  [`cold-run-report.json`](../demo/live-evidence/android/cold-run-report.json), and
  [`warm-run-report.json`](../demo/live-evidence/android/warm-run-report.json). Independent validation
  is `2026-11-22T20:27:33.668Z`; passing exact-replay validation is `2026-11-22T20:28:37.591Z`.
- **No new device run:** no emulator/simulator, credentials, external service, or other worktree was
  used. The graph copies only stable `screen_*` Android resource IDs and tap IDs; it does not copy raw
  text, fixture inputs, screenshots, recordings, device/environment details, or logs. Its accepted
  cold/warm path has no failed branch, so `failedBranches` is empty rather than fabricated or mixed
  with WALK-020's separate repair fixture. Failed-branch behavior is deterministic-fixture evidence.
- Core orchestration, cache identity, `LocalWalkStore`, runner, exploration, and repair behavior were
  not changed. This graph is diagnostic-only, not independently executable or complete app coverage.

Portable verification: `bun run lint`, `bun test --timeout=10000`, `bun run build`,
`bun scripts/derive-walk017-route-graph.ts --check`, and `git diff --check` passed. The release
artifact validator was run after committing on the clean checkout; see the handoff for its commit-bound
result. No post-WALK-020 Android execution is claimed.

