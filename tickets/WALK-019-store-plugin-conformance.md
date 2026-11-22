---
id: WALK-019
title: Prove store independence with a second implementation
milestone: P1 plugin conformance
priority: P1
status: deferred
depends_on: [WALK-008]
---

# WALK-019 — Prove store independence with a second implementation

## Outcome

Validate the `WalkStore` boundary using the same behavior tests against two implementations.

## Checklist

- [ ] Extract shared contract tests for identity, retrieval, persistence, corruption, history, and
  replacement failures.
- [ ] Run them unchanged against local filesystem storage and a second small implementation.
- [ ] Preserve zero-model cache hits and atomic last-known-good behavior in both implementations.
- [ ] Keep the second store local/simple; no hosted account system or remote service is required.
- [ ] Document durability, concurrency, setup, and limitations honestly.

## Acceptance evidence

Link the shared suite's passing output for both stores and one orchestration run using each. A mock
that merely satisfies types does not count as a second storage implementation.

