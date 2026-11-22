---
id: WALK-008
title: Build the headless CLI around production plugin contracts
milestone: M6A–M6B
priority: P0
status: queued
depends_on: [WALK-003, WALK-004, WALK-005, WALK-006, WALK-007]
---

# WALK-008 — Build the headless CLI around production plugin contracts

## Outcome

Move the proven lifecycle behind small TypeScript interfaces and expose a reliable headless product
without deleting or weakening the Android spike.

## Checklist

- [ ] Define core goal, walk, run result, failure, artifact, and metrics contracts independently of
  OpenClaw, Maestro, React, and a particular storage implementation.
- [ ] Put `WalkStore`, `WalkRunner`, `WalkExplorer`, `SelectorRepairer`, and `RunReporter` behind
  injected interfaces; shared orchestration contains no per-game/per-demo or per-platform switches.
- [ ] Add workspace/package boundaries only as needed by the working vertical slice.
- [ ] Provide `walkcache explore <goal>`, `run <goal>`, `remember`, `list`, and `inspect <report>` with
  useful help, configuration, output, and documented exit codes.
- [ ] Make `run` check local memory before explorer invocation; cache hits always bypass the model.
- [ ] On a miss, use the verified cold lifecycle; on repairable staleness, invoke only WALK-007's
  bounded policy; on regression/unknown failure, report and stop.
- [ ] Print cache status, model calls, duration, assertions, repair count, YAML/report paths, and diffs.
- [ ] Preserve independent Maestro YAML execution in CI without WalkCache or an LLM.
- [ ] Add orchestration tests proving cache hits construct no explorer/model client.

## Acceptance evidence

From documented configuration, link cold, warm, miss, stale-selector repair, and product-regression
CLI transcripts/reports. Run lint, type-check, focused tests, and the original Android flow. Include
a constructor/spy test proving zero explorer invocation on a warm hit.

