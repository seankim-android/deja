---
id: WALK-016
title: Add exact-match-first semantic goal retrieval
milestone: P1 retrieval
priority: P1
status: done
depends_on: [WALK-008]
---

# WALK-016 — Add exact-match-first semantic goal retrieval

## Outcome

Reuse a verified walk for semantically equivalent goals without weakening cache correctness.

## Checklist

- [x] Preserve deterministic exact identity as the first lookup and fallback.
- [x] Rank only walks matching app, platform, schema, build, and verification constraints.
- [x] Show goal similarity and selected walk; ambiguous/low-confidence matches return a miss.
- [x] Keep embedding/model calls out of exact cache hits and report semantic lookup metrics separately.
- [x] Test equivalent, unrelated, contradictory, and adversarial goals plus missing embedding service.
- [x] Use a local/simple implementation; no remote vector database in the hackathon MVP.

## Acceptance evidence

Relevance/fallback tests: [controlled core vectors](../packages/core/test/semantic.test.ts),
[deterministic local embeddings](../packages/semantic-local/test/local-embeddings.test.ts), and
[real local-store identity/checksum/latest-validation integration](../packages/store-local/test/semantic-retrieval.test.ts).
Orchestration traces prove exact-hit provider laziness, selected-identity validation, failed semantic
replay, provider fallback, and adversarial rejection; see
[orchestration tests](../packages/core/test/orchestration.test.ts) and
[CLI report/output integration](../apps/cli/test/cli.test.ts). The
[acceptance matrix and honest limitations](../spikes/001-android-deja/evidence/walk016-semantic-retrieval.md)
link the focused 47-test/271-assertion result and the
[complete committed regression trace](../spikes/001-android-deja/evidence/walk016-all-tests.txt).

The proof uses `local-lexical-v1`, not generalized semantic correctness. It is opt-in, exact-first,
limited to exact `not-found` misses, constrains candidates through the real store before ranking,
and always replays selected YAML. Missing providers use ordinary verified miss/explore behavior;
corrupt memory still fails loudly. See [Architecture](../docs/ARCHITECTURE.md#exact-first-semantic-proof-walk-016).

