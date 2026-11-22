---
id: WALK-017
title: Add a minimal route graph and screen-state fingerprints
milestone: P1 navigation memory
priority: P1
status: deferred
depends_on: [WALK-008]
---

# WALK-017 — Add a minimal route graph and screen-state fingerprints

## Outcome

Record verified navigation structure to improve diagnosis and future composition without turning an
incomplete graph into a correctness claim.

## Checklist

- [ ] Define bounded screen fingerprints from stable non-secret observations.
- [ ] Record nodes, verified actions/transitions, app/build identity, and validation timestamps.
- [ ] Store failed branches separately from successful routes.
- [ ] Detect changed screens conservatively and fall back to existing verified replay/exploration.
- [ ] Avoid raw credentials, personal data, screenshots, or unrestricted environment dumps.
- [ ] Add deterministic fixtures for repeated, ambiguous, changed, and unknown screens.

## Acceptance evidence

Link a small Android route graph, schema validation, and tests showing unknown/changed states cannot
produce a false cache hit. Keep route graphs out of the critical P0 path.

