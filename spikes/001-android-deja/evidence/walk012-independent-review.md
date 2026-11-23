# WALK-012 independent clean-context review

Review target: `f9a4261` (README quick start, architecture and release documentation). A fresh
isolated reviewer performed this review; the WalkCache author did not execute it. The reviewer made
no tracked edits, ran no model exploration, used no personal credentials, and restored the original
cache and stable fixture afterward. This is a curated review report, not a raw terminal transcript.

## Independent Android quick-start result: pass

Environment: Bun 1.4.4, Maestro 2.12.0, OpenJDK 21.0.13, Android 14/API 34. The reviewer started
the existing dedicated `walkcache_api_34` emulator (`emulator-5554`), built and installed the fixture,
selected stable mode, and followed the README from a quarantined clean cache.

- Build: success; 33 Gradle tasks; reported 17 seconds.
- APK install: `Success`; stable fixture mode confirmed.
- `remember checkout --flow evidence/repaired-checkout.yaml`: exit 0, `remembered`, 12 assertions,
  0 model calls, 36,948 ms.
- Warm `run checkout`: exit 0, `passed`, cache `hit`, 12 assertions, 0 model calls, 0 repairs,
  33,229 ms.
- `inspect .walkcache/reports/latest.json`: exit 0 and the same passed/cache-hit result.
- Stored YAML SHA-256 begins `e09660f4` and ends `b12b478`, matching the retained verified flow.
- `list`: exit 0 with one verified Android walk. `--help`: exit 0. No command: exit 2.
- Original cache aggregate hash matched before/after; fixture stayed stable and the reviewer stopped
  the emulator it started. Dependencies already existed, so the supplied checkout did not require a
  fresh clone/install. Web/iOS and comparative performance were outside this focused review.

## Findings returned to author

1. **Medium — ticket evidence links were broken.** Five WALK-012 links and thirteen WALK-010 links
   used `../../` from `tickets/`, resolving above the repository. Repository-wide inspection found
   the same defect in WALK-008/009/013/014/015 too. Fixed to `../` in all seven affected tickets; a
   tracked-Markdown local-link validator was added to portable artifact checks.
2. **Medium — executable-launch exit documentation disagreed with behavior.** A controlled command
   with `maestro` absent returned outcome `rejected`, failure `runner`, and exit **1**, while README
   and built-in help incorrectly grouped executable errors with exit **2**. Behavior is unchanged:
   docs now classify runner/executable-launch failures as exit 1, and a focused CLI test reproduces
   the launch failure and exit contract. Code 2 remains direct usage/config/report/I/O failures.
3. **Low — dead Vite starter assets.** `src/assets/react.svg` and `src/assets/vite.svg` were tracked
   without references. Both were removed; no UI reference was removed or behavior changed.

The reviewer found no missing local links in the root README, architecture guide or CLI README.
Support boundaries, artifact layouts, configured source roots/security, schemas and historical
metric claims matched the inspected code. Follow-up verification on the corrected revision is
recorded separately after the fixes are checked; this report itself is the initial NOT-PASS review.

## Follow-up verification: PASS

The first follow-up confirmed all three fixes but rejected an incomplete test transcript: Bun wrote
the detailed passing results to stderr while the pipeline captured only stdout. The author preserved
the actual complete rerun with both streams redirected. Final independent verdict:

**PASS — all three findings and the WALK-012 acceptance gate are resolved.**

- Scoped ticket links: 115 local links checked, zero missing; the release validator now checks all
  tracked Markdown links.
- Missing-Maestro `remember`: `runner` failure with CLI exit 1, matching corrected docs/help.
- Focused CLI rerun: 5 pass / 0 fail / 64 assertions. Lint and production build passed.
- Unused Vite SVGs deleted. No further documentation acceptance blocker remained.

The author then ran the complete portable suite on the integrated iOS revision: **106 tests / 626
assertions passed**. No reviewer reran Android merely to reproduce the same quick start; the
independent device evidence above and the separate final focused test evidence jointly close the gate.
