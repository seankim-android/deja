---
id: WALK-007
title: Enforce bounded repair and the product-regression guard
milestone: M4B–M4C
priority: P0
status: queued
depends_on: [WALK-006]
---

# WALK-007 — Enforce bounded repair and the product-regression guard

## Outcome

Productionize the validated two-file selector patch under strict limits and complete the missing
negative experiment: a genuine product regression must stay red.

## Checklist

- [ ] Permit repair only for WALK-006's explicitly repairable category; reject product assertions,
  ambiguous failures, timeouts, and infrastructure errors.
- [ ] Allow writes only under configured source roots and permitted file types; reject path escapes.
- [ ] Limit repair to one attempt, one time budget, and the affected component/selector files.
- [ ] Display the proposed diff and affected files; do not auto-commit or create a pull request.
- [ ] Rebuild, recompute build identity, and verify the full journey before replacing cached memory.
- [ ] Preserve the previous flow, failed attempts, patch, build report, and verification artifacts.
- [ ] Add a deterministic product-regression fixture (for example, incorrect final total/confirmation).
- [ ] Prove the repair path neither changes source nor rewrites assertions for that fixture and exits
  nonzero with the last known-good memory intact.

## Acceptance evidence

Link the bounded successful repair report and diff plus the negative regression report, unchanged
source diff, and preserved-memory check. Retain WALK-001's original repair as historical evidence;
do not represent it as proof of generalized autonomous repair.

