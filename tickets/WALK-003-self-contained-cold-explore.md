---
id: WALK-003
title: Add a self-contained cold explore goal lifecycle
milestone: M2B–M2C
priority: P0
status: queued
depends_on: [WALK-002]
---

# WALK-003 — Add a self-contained cold `explore <goal>` lifecycle

## Outcome

Replace the manually coordinated isolated agent session with one reproducible command that explores
from a natural-language goal, independently verifies the candidate, and persists it only on success.

## Checklist

- [ ] Add `walkcache explore <goal>` with explicit app, platform, build, and workspace configuration.
- [ ] Use a documented non-interactive OpenClaw/Kimi invocation and Maestro MCP transport.
- [ ] Deny the explorer the reference flow and app source; provide only the goal and allowed fixture
  credentials/configuration.
- [ ] Start from clean app state, record MCP activity, and keep failed branches in attempt artifacts.
- [ ] Write candidate YAML to attempts, never directly to successful flow storage.
- [ ] Verify the complete candidate through WALK-002 from a fresh `clearState` launch.
- [ ] Persist through the atomic store only after independent verification passes.
- [ ] Report exploration duration, verification duration, MCP calls, and model calls when available;
  emit `null`/`unknown` rather than guessing unavailable values.
- [ ] Prove a rejected candidate does not replace the last known-good flow.

## Acceptance evidence

From a clean candidate/cache state, run one command for the Android checkout goal. Link the authored
YAML, MCP activity log, independently passing report, and stored metadata. Repeat with a deliberately
invalid candidate and show that no successful memory was created or replaced.

## Non-goals

Warm replay optimization, semantic retrieval, source repair, and support for additional platforms.

