---
id: WALK-010
title: Prove the same journey on web through its adapter
milestone: M5B
priority: P0
status: queued
depends_on: [WALK-005, WALK-007, WALK-008]
---

# WALK-010 — Prove the same journey on web through its adapter

## Outcome

Add web only after Android correctness gates pass, and claim support only from a real Chromium run.

## Checklist

- [ ] Build a deterministic secret-free web login → add item → checkout fixture with stable DOM
  selectors and meaningful final assertions.
- [ ] Implement selector/build identity through the platform adapter, not shared per-demo conditionals.
- [ ] Check the installed Maestro version's actual web support and document beta/Chromium constraints.
- [ ] Run cold exploration, independent verification, warm replay, invalidation, and regression tests
  or explicitly narrow the claimed support to the behaviors actually demonstrated.
- [ ] Record platform, browser, build, and adapter identity in cache keys and reports.
- [ ] Keep Android evidence and commands reproducible after shared changes.

## Acceptance evidence

Link actual Chromium commands, YAML, reports, and supported behavior matrix. If tooling blocks the
run, record the exact error/version and mark web unsupported in demo/docs; shared YAML alone is not
proof of web support.

