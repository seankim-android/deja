---
id: WALK-012
title: Prepare accurate OSS documentation and package metadata
milestone: M7 documentation
priority: P0
status: done
depends_on: [WALK-008]
---

# WALK-012 — Prepare accurate OSS documentation and package metadata

## Outcome

Make WalkCache understandable and reproducible by another developer without overstating platform
support, autonomy, security, or performance.

## Checklist

- [x] Add Apache-2.0 license and accurate package/repository metadata.
- [x] Rewrite README around WalkCache with prerequisites and a five-minute Android quick start.
- [x] Document all CLI commands, configuration, exit codes, artifacts, and clean-state setup.
- [x] Add `docs/ARCHITECTURE.md` for plugin boundaries, cache keys, memory schema, and atomic writes.
- [x] Document secret handling, configured source roots, bounded repair, no auto-commit behavior, and
  the stale-selector versus product-regression rule.
- [x] Add an honest support matrix and limitations, including Maestro web beta/Chromium constraints
  only after checking actual behavior.
- [x] Distinguish historical spike measurements from current end-to-end metrics.
- [x] Remove starter instructions, dead content, generated artifacts, and unverified claims.
- [x] Link the repo-local ticket backlog and explain how evidence closes a ticket.

## Acceptance evidence

Have a clean-context review follow only the README to reach the first verified warm replay. Link docs
and review notes; platform entries match WALK-010/WALK-011 evidence or are marked unsupported.

Evidence:

- [Apache-2.0 license](../LICENSE) and [root package metadata](../package.json), including
  package name/version/description, repository, homepage, issues, keywords, license, and Bun engine.
- [Root README](../README.md): WalkCache product boundary, prerequisites, model-free five-minute
  Android `remember` → warm replay path, all CLI commands/options/configuration/exit codes,
  artifact/clean-state handling, safety, support matrix, limitations, development, and evidence workflow.
- [Architecture guide](../docs/ARCHITECTURE.md): lifecycle, plugin/adapter boundaries, exact cache
  identity, schema-2 memory and schema-1 run reports, atomic replacement, artifact boundaries, failure
  policy, metric scopes, and secret handling.
- [README quick-start runtime review](../spikes/001-android-walkcache/evidence/walk012-readme-review.txt):
  prerequisite/device versions and author-executed clean-cache build/install/`remember`/warm
  `run`/`inspect` commands with redacted results. No cold model run, Web run, iOS run, or comparative
  timing benchmark was part of this review.
- Support entries match [WALK-010](WALK-010-verified-web-proof.md) and
  [WALK-011](WALK-011-verified-ios-proof.md): Android cold/warm/repair verified; Chromium and native
  iOS reviewed-YAML `remember`/warm/invalidation/regression verified, with cold exploration and repair
  explicitly unsupported on both. No current-CLI measured wall-clock speedup is claimed;
  WALK-005 remains historical.
- Local checks: `bun run lint` (0 warnings/errors), `bun test --timeout=10000`
  (106 tests / 626 assertions passed; pre-existing React `act(...)` warnings), `bun run build`
  (TypeScript + Vite passed), and `git diff --check` passed.

- [Independent clean-context review and fix verification](../spikes/001-android-walkcache/evidence/walk012-independent-review.md):
  fresh reviewer independently built/installed Android, followed README from a quarantined cache,
  and passed `remember` → warm `run` → `inspect` with 12 assertions and zero model calls. Initial
  NOT-PASS findings were broken ticket links, executable-launch exit documentation, and dead starter
  SVGs. The author fixed all seven affected ticket files, added local Markdown validation and a
  focused missing-Maestro exit-1 test, and removed both unreferenced assets. Final reviewer verdict:
  **PASS, all three findings and the WALK-012 acceptance gate resolved**. The earlier failed
  clean-context attempt is superseded by this completed review; the author-executed quick start was
  never counted as independent review.
- Evidence: [focused CLI test](../spikes/001-android-walkcache/evidence/walk012-cli-tests.txt)
  (5 tests / 64 assertions), [full suite](../spikes/001-android-walkcache/evidence/walk012-all-tests.txt)
  (106 tests / 626 assertions), [lint](../spikes/001-android-walkcache/evidence/walk012-lint.txt),
  and [build](../spikes/001-android-walkcache/evidence/walk012-build.txt).

