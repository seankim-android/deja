---
id: WALK-009
title: Replace the Vite starter with an optional local run viewer
milestone: M6C
priority: P0
status: done
depends_on: [WALK-008]
---

# WALK-009 — Replace the Vite starter with an optional local run viewer

## Outcome

Let a fresh viewer understand cold, warm, failure, and repair states without reading source, while
keeping the CLI fully functional when the dashboard is unavailable.

## Checklist

- [x] Replace starter branding/content with a small local WalkCache run and artifact viewer.
- [x] Show elapsed time, model calls, cache status, assertions, failures, and repair count.
- [x] Display generated YAML, validation history, and proposed source diffs with safe path handling.
- [x] Redact secrets and reject attempts to load arbitrary local paths or credential-bearing files.
- [x] Default to read-only local reports; any command execution must be explicit and locally scoped.
- [x] Handle empty, malformed, missing, and in-progress reports without fabricating a successful state.
- [x] Add a clear headless CLI fallback and verify the dashboard is not required for any lifecycle.
- [x] Add focused UI tests and readable terminal-quality styling; animations are cut first.

## Acceptance evidence

Link screenshots or UI test artifacts for cold, warm, repair, and regression states plus the headless
CLI run with the viewer stopped. Screenshots contain fixture data only.

Evidence:

- Usage, read-only API boundaries, and CLI fallback: [`viewer/README.md`](../../viewer/README.md).
- Actual Chromium screenshots with fixture data only:
  [`cold`](../../spikes/001-android-walkcache/evidence/walk009-cold.png),
  [`warm`](../../spikes/001-android-walkcache/evidence/walk009-warm.png),
  [`repair diff`](../../spikes/001-android-walkcache/evidence/walk009-repair.png), and
  [`product regression`](../../spikes/001-android-walkcache/evidence/walk009-regression.png).
- Focused UI/API artifacts:
  [`16 passing tests / 69 assertions`](../../spikes/001-android-walkcache/evidence/walk009-ui-test-artifacts.txt)
  covering all four lifecycle states, empty/malformed/missing/in-progress behavior, redaction,
  traversal/credential-path rejection, symlink escapes, and view-independent CLI fallback.
- Independent YAML verification with no viewer process:
  [`remember transcript`](../../spikes/001-android-walkcache/evidence/walk009-headless-remember.txt) —
  passed in 26,788 ms with 0 model calls and 12 assertions.
- Warm CLI replay with no viewer process:
  [`transcript`](../../spikes/001-android-walkcache/evidence/walk009-headless-no-viewer.txt) — cache hit,
  0 model calls, 12 assertions, 27,714 ms, 0 repairs, and exit 0. An initial disconnected-device run
  was not accepted; the known-good YAML was independently restored before this warm proof.
- Full checks: [`96 tests / 569 assertions`](../../spikes/001-android-walkcache/evidence/walk009-all-tests.txt),
  [`lint`](../../spikes/001-android-walkcache/evidence/walk009-lint.txt), and
  [`type-check / production build`](../../spikes/001-android-walkcache/evidence/walk009-build.txt).
  No animation was added. No lifecycle command is exposed through the viewer API.

