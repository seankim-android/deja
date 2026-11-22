---
id: WALK-005
title: Capture comparable cold and warm metrics
milestone: M3C
priority: P0
status: queued
depends_on: [WALK-003, WALK-004]
---

# WALK-005 — Capture comparable cold and warm metrics

## Outcome

Measure the complete cold and warm lifecycle under equivalent conditions, while keeping model
avoidance separate from execution speed.

## Checklist

- [ ] Define cold start/end boundaries including exploration, verification, persistence, and device
  setup when it is part of the command.
- [ ] Run cold and warm against the same build, goal, device state, and meaningful assertions.
- [ ] Record wall time, runner time, MCP calls, model calls, cache status, and verification result.
- [ ] Obtain model-call counts from actual runtime instrumentation; never equate MCP calls with model
  calls or fill missing values with zero.
- [ ] Repeat enough controlled runs to identify setup noise; report raw samples, not only a best case.
- [ ] Save a side-by-side JSON/Markdown artifact suitable for the demo.
- [ ] Keep the existing 24.824s and 24.510s warm samples as historical evidence, not a speedup claim.

## Acceptance evidence

Link raw cold/warm reports and the comparison artifact. Every displayed number must be traceable to a
report; explicitly state if warm replay avoids model calls but Maestro execution time is similar.

