---
id: WALK-009
title: Replace the Vite starter with an optional local run viewer
milestone: M6C
priority: P0
status: queued
depends_on: [WALK-008]
---

# WALK-009 — Replace the Vite starter with an optional local run viewer

## Outcome

Let a fresh viewer understand cold, warm, failure, and repair states without reading source, while
keeping the CLI fully functional when the dashboard is unavailable.

## Checklist

- [ ] Replace starter branding/content with a small local WalkCache run and artifact viewer.
- [ ] Show elapsed time, model calls, cache status, assertions, failures, and repair count.
- [ ] Display generated YAML, validation history, and proposed source diffs with safe path handling.
- [ ] Redact secrets and reject attempts to load arbitrary local paths or credential-bearing files.
- [ ] Default to read-only local reports; any command execution must be explicit and locally scoped.
- [ ] Handle empty, malformed, missing, and in-progress reports without fabricating a successful state.
- [ ] Add a clear headless CLI fallback and verify the dashboard is not required for any lifecycle.
- [ ] Add focused UI tests and readable terminal-quality styling; animations are cut first.

## Acceptance evidence

Link screenshots or UI test artifacts for cold, warm, repair, and regression states plus the headless
CLI run with the viewer stopped. Screenshots contain fixture data only.

