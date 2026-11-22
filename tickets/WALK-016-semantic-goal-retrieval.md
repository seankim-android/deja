---
id: WALK-016
title: Add exact-match-first semantic goal retrieval
milestone: P1 retrieval
priority: P1
status: deferred
depends_on: [WALK-008]
---

# WALK-016 — Add exact-match-first semantic goal retrieval

## Outcome

Reuse a verified walk for semantically equivalent goals without weakening cache correctness.

## Checklist

- [ ] Preserve deterministic exact identity as the first lookup and fallback.
- [ ] Rank only walks matching app, platform, schema, build, and verification constraints.
- [ ] Show goal similarity and selected walk; ambiguous/low-confidence matches return a miss.
- [ ] Keep embedding/model calls out of exact cache hits and report semantic lookup metrics separately.
- [ ] Test equivalent, unrelated, contradictory, and adversarial goals plus missing embedding service.
- [ ] Use a local/simple implementation; no remote vector database in the hackathon MVP.

## Acceptance evidence

Link relevance/fallback tests and traces proving a wrong semantic match cannot replace exact identity
or bypass independent verification. Stay deferred until all relevant P0 gates pass.

