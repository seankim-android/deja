---
id: WALK-013
title: Pass portable tests and clean-checkout CI
milestone: M7 quality gate
priority: P0
status: done
depends_on: [WALK-012]
---

# WALK-013 — Pass portable tests and clean-checkout CI

## Outcome

Verify the repository from clean state, keep machine-specific emulator paths out of portable checks,
and ensure CI exercises every check it claims.

## Checklist

- [x] Add focused unit tests for orchestration, storage, runner, metrics, and failure classification.
- [x] Separate portable tests from Android/web/iOS integration jobs and label their requirements.
- [x] Run dependency install, lint, TypeScript, tests, build, and artifact/JSON validation from a clean
  checkout with no ignored files carried over.
- [x] Ensure GitHub Actions runs and passes every portable check on the relevant branch.
- [x] Execute all claimed platform flows with recorded commands, or exclude them honestly from CI.
- [x] Verify standalone Maestro YAML runs without WalkCache or model access.
- [x] Scan tracked files and demo artifacts for secrets, personal data, machine-specific leaks, and
  accidental build/temporary outputs.
- [x] Confirm `main` branch history and release state before recording; never imply local commits are
  published or merged when they are not.

## Acceptance evidence

Link clean-checkout command output and GitHub Actions run URLs, plus integration reports. Explain any
intentionally non-CI platform gate and its exact local reproduction command.

Evidence:

- [Portable CI guide](../docs/CI.md) and [workflow](../.github/workflows/portable-checks.yml):
  Linux-only dependency/lint/test/type/build/artifact checks with read-only contents permission;
  Android/Chromium stay explicit local integration gates, and iOS/model/device execution is not claimed.
- [Clean clone transcript](../spikes/001-android-walkcache/evidence/walk013-clean-checkout.txt):
  `b827311` with no ignored files carried over; locked install, lint, 101 tests / 599 assertions,
  TypeScript/Vite build, JSON/artifact validation and unchanged final checkout all passed.
- [Artifact validation](../spikes/001-android-walkcache/evidence/walk013-artifact-validation.txt):
  287 tracked files, 279 text files and 51 strict JSON files checked; no generated artifacts,
  machine-specific home paths or scanned token/key shapes, and the committed checkout stayed unchanged.
- [Standalone Android YAML](../spikes/001-android-walkcache/evidence/walk013-standalone-android.txt):
  direct Maestro execution completed all 19 steps and exited 0 with no WalkCache CLI/cache/viewer/model.
  Web reproduction is in [`spikes/002`](../spikes/002-web-walkcache/README.md); both are intentionally
  absent from portable CI rather than faked. Old curated evidence's absolute local paths were redacted
  without changing outcomes. Detector setup rejected the intentional Gradle wrapper, JSONC tsconfigs,
  and its own literal scanner string before being corrected; final validation passes on clean clones.
- Release history was checked on clean `main...origin/main` at `b827311` before this evidence closeout.
  Demo asset commit `8c301cd` was already pushed before its Slack upload; no local commit is presented
  as merged/public before its actual push.
- [GitHub Actions run 45060550371](https://github.com/seankim-android/kimi-hackathon/actions/runs/45060550371)
  ([structured receipt](../spikes/001-android-walkcache/evidence/walk013-github-actions.json)): final
  `success` on `main` commit `b8273117ca671b1fb70b4d56f3c2d2b42e12ed28`; every setup/checkout/install/
  lint/test/build/artifact/revision step passed in 15 seconds. Earlier validator-setup runs failed and
  were corrected rather than represented as green. The independent README-review step remains tracked
  separately under WALK-012; it is not represented as a portable CI result.

