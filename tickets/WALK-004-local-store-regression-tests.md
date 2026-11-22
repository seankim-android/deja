---
id: WALK-004
title: Add local-store regression and atomicity tests
milestone: M3A–M3C
priority: P0
status: queued
depends_on: [WALK-002]
---

# WALK-004 — Add local-store regression and atomicity tests

## Outcome

Make cache persistence conservative and testable so corruption, stale builds, or interrupted writes
never become false passes or destroy reusable memory.

## Checklist

- [ ] Test exact goal normalization plus app, platform, schema, flow-checksum, and build identity.
- [ ] Cover first-run miss, valid hit, stale build, wrong app/platform, and missing flow.
- [ ] Cover malformed JSON, unsupported schema, invalid entries, and flow path escapes.
- [ ] Simulate interruption before rename and prove temporary files are not loaded as memory.
- [ ] Prove failed verification/replacement preserves the previous known-good flow and metadata.
- [ ] Preserve validation history without treating a failed validation as a successful walk.
- [ ] Use isolated temporary directories and deterministic fixtures; require no emulator for unit tests.
- [ ] Document recovery behavior: report invalid memory and require exploration rather than silently
  converting corruption into a pass.

## Acceptance evidence

Link the focused test command and passing output, including test names for every case above. Retain
one real Android stale-build report from WALK-001 as integration evidence.

