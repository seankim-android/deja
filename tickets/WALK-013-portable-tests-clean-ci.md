---
id: WALK-013
title: Pass portable tests and clean-checkout CI
milestone: M7 quality gate
priority: P0
status: in_progress
depends_on: [WALK-012]
---

# WALK-013 — Pass portable tests and clean-checkout CI

## Outcome

Verify the repository from clean state, keep machine-specific emulator paths out of portable checks,
and ensure CI exercises every check it claims.

## Checklist

- [ ] Add focused unit tests for orchestration, storage, runner, metrics, and failure classification.
- [ ] Separate portable tests from Android/web/iOS integration jobs and label their requirements.
- [ ] Run dependency install, lint, TypeScript, tests, build, and artifact/JSON validation from a clean
  checkout with no ignored files carried over.
- [ ] Ensure GitHub Actions runs and passes every portable check on the relevant branch.
- [ ] Execute all claimed platform flows with recorded commands, or exclude them honestly from CI.
- [ ] Verify standalone Maestro YAML runs without WalkCache or model access.
- [ ] Scan tracked files and demo artifacts for secrets, personal data, machine-specific leaks, and
  accidental build/temporary outputs.
- [ ] Confirm `main` branch history and release state before recording; never imply local commits are
  published or merged when they are not.

## Acceptance evidence

Link clean-checkout command output and GitHub Actions run URLs, plus integration reports. Explain any
intentionally non-CI platform gate and its exact local reproduction command.

