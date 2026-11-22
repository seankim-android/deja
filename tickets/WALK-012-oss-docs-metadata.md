---
id: WALK-012
title: Prepare accurate OSS documentation and package metadata
milestone: M7 documentation
priority: P0
status: queued
depends_on: [WALK-008]
---

# WALK-012 — Prepare accurate OSS documentation and package metadata

## Outcome

Make WalkCache understandable and reproducible by another developer without overstating platform
support, autonomy, security, or performance.

## Checklist

- [ ] Add Apache-2.0 license and accurate package/repository metadata.
- [ ] Rewrite README around WalkCache with prerequisites and a five-minute Android quick start.
- [ ] Document all CLI commands, configuration, exit codes, artifacts, and clean-state setup.
- [ ] Add `docs/ARCHITECTURE.md` for plugin boundaries, cache keys, memory schema, and atomic writes.
- [ ] Document secret handling, configured source roots, bounded repair, no auto-commit behavior, and
  the stale-selector versus product-regression rule.
- [ ] Add an honest support matrix and limitations, including Maestro web beta/Chromium constraints
  only after checking actual behavior.
- [ ] Distinguish historical spike measurements from current end-to-end metrics.
- [ ] Remove starter instructions, dead content, generated artifacts, and unverified claims.
- [ ] Link the repo-local ticket backlog and explain how evidence closes a ticket.

## Acceptance evidence

Have a clean-context review follow only the README to reach the first verified warm replay. Link docs
and review notes; platform entries match WALK-010/WALK-011 evidence or are marked unsupported.

