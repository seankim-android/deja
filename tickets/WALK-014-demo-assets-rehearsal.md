---
id: WALK-014
title: Prepare deterministic demo assets and rehearse twice
milestone: M7C
priority: P0
status: queued
depends_on: [WALK-005, WALK-007, WALK-008, WALK-013]
---

# WALK-014 — Prepare deterministic demo assets and rehearse twice

## Outcome

Create a reliable, readable demo whose spoken claims match raw evidence, with a fallback that does
not depend on live model or simulator luck.

## Checklist

- [ ] Script a ≤60-second sequence: cold exploration, saved YAML, warm zero-model replay, stale
  selector, bounded repair, and honest support/architecture conclusion.
- [ ] Capture readable terminal/dashboard output and retain the raw JSON/YAML/diff for every claim.
- [ ] Prepare a deterministic fallback recording from reproducible fixture runs.
- [ ] Do not state or imply “instant” wall-clock replay unless WALK-005 supports the comparison.
- [ ] Show only verified platforms and measured model-call counts; label unknown values.
- [ ] Review every frame for credentials, personal data, tokens, absolute private paths, and secrets.
- [ ] Rehearse end to end twice, timing each run and checking every spoken claim against evidence.
- [ ] Record any failed take and correct the script or product before final capture.

## Acceptance evidence

Link the script, fallback, raw artifacts, two rehearsal timings, and review checklists. The gate is
complete only when both rehearsals stay at or below 60 seconds and all claims are supported.

