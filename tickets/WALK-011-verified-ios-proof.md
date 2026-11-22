---
id: WALK-011
title: Prove the same journey on iOS through its adapter
milestone: M5C
priority: P0
status: queued
depends_on: [WALK-005, WALK-007, WALK-008]
---

# WALK-011 — Prove the same journey on iOS through its adapter

## Outcome

Validate iOS independently without delaying or weakening the Android proof.

## Checklist

- [ ] Use the smallest deterministic iOS fixture that supports login → add item → checkout.
- [ ] Add stable accessibility/resource identifiers and meaningful final assertions.
- [ ] Borrow Blankaday's general ID-based Maestro conventions where useful, never its product-specific
  flows, credentials, personal data, or unrelated app code.
- [ ] Implement platform and build identity through the adapter and use a dedicated simulator.
- [ ] Run cold exploration, independent verification, warm replay, invalidation, and regression tests
  or explicitly narrow support to demonstrated behavior.
- [ ] Re-run Android after shared orchestration changes.

## Acceptance evidence

Link simulator/device OS, exact commands, YAML, and result reports. If setup or tooling is blocked,
record the concrete blocker and remove iOS from the claimed demo matrix; Android evidence does not
establish iOS support.

