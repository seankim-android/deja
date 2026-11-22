---
id: WALK-018
title: Reuse a verified login subflow in another checkout goal
milestone: P1 composition
priority: P1
status: deferred
depends_on: [WALK-017]
---

# WALK-018 — Reuse a verified login subflow in another checkout goal

## Outcome

Compose a new journey from an independently verified login segment while preserving full-journey
assertions and cache safety.

## Checklist

- [ ] Define typed subflow inputs/outputs, app/build identity, preconditions, and postconditions.
- [ ] Save reusable segments only after independent verification.
- [ ] Compose login with a materially different checkout goal without duplicating login steps.
- [ ] Verify the complete composed journey from clean state before saving it as a successful walk.
- [ ] Ensure failed composition cannot overwrite either the login segment or previous goal memory.
- [ ] Report reused versus newly explored steps and any model calls separately.

## Acceptance evidence

Link the login subflow, composed YAML, clean-state verification report, and failure-preservation test.
No claim of general route planning beyond the demonstrated composition.

