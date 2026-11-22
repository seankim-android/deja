---
id: WALK-006
title: Classify stale selectors separately from product regressions
milestone: M4A
priority: P0
status: queued
depends_on: [WALK-002]
---

# WALK-006 — Classify stale selectors separately from product regressions

## Outcome

Make invalidation repeatable and ensure repair is considered only for selector/navigation failures,
never for a meaningful final product assertion.

## Checklist

- [ ] Add a controlled fixture mode/source change that makes only `checkout_button` stale.
- [ ] Preserve WALK-001's curated failing evidence and make the experiment reproducible from a clean
  Android build without hand-editing ignored memory.
- [ ] Parse runner output into conservative `selector`, `navigation`, `product_assertion`, `runner`,
  `timeout`, and `unknown` failure categories with the failing step and raw artifact reference.
- [ ] Treat heuristics as advisory; ambiguous output stays `unknown`, never automatically repairable.
- [ ] Keep cache-hit replay at zero model calls, including when it fails.
- [ ] Add positive and negative fixtures for each supported classification.
- [ ] Do not weaken or rewrite assertions to match changed product behavior.

## Acceptance evidence

Link a test matrix and real Android artifacts showing the stale run fails only at Checkout, while a
wrong order-confirmation assertion is classified as a product assertion and remains a failure.

## Non-goals

No model-generated patch in this ticket, no generalized natural-language diagnosis, and no claim that
log parsing proves the root cause of every application failure.

