# P1 final integration verification

This covers the combined local branch containing WALK-016, WALK-017, WALK-018 and WALK-019.
The feature/review-fix commits before this evidence-only closeout end at `e7252d8994b30fae54e07f09f8d9966ae7727281`.

## Combined portable checks

- `bun test --timeout=10000`: **221 pass / 0 fail / 1,129 assertions across 26 files**;
  complete actual output: [p1-final-all-tests.txt](p1-final-all-tests.txt).
- `bun run lint`: 0 warnings / 0 errors.
- `bun run build`: TypeScript project references and Vite production build passed.
- `bun scripts/derive-walk017-route-graph.ts --check`: byte-exact evidence re-derivation and schema
  validation passed; four nodes, three verified transitions, no fabricated failed Android branch.
- `bun scripts/derive-walk018-composition.ts --check`: curated login/continuation/composed YAML and
  assertions re-derived exactly.
- `bun scripts/verify-walk018-composition.ts --check`: fresh deterministic independent login
  verification, clean-state composed journey, exact replay, and failed-composition preservation
  passed. Eleven login steps were reused through a `runFlow` reference; 14 exact final-text
  assertions passed; all new composition/verification/replay model-call counts were zero.
  [Wrong final text](walk018/walk018-wrong-final-text.log) is now a product assertion failure with no
  goal persistence, and a [late second clean launch](walk018/walk018-late-second-clear-launch.log)
  is rejected without changing the segment. Same-directory references pass while parent, normalized,
  absolute, backslash and containing-directory escapes are rejected. The
  [canonical-output containment log](walk018/walk018-canonical-output-containment.log) additionally
  proves five preexisting filesystem attacks reject before any runner call: goal-directory and
  composed-root symlinks, dangling symlinks, and non-directory traversal. External sentinels and
  last-known-good walk/segment storage remain unchanged, while legitimate nested output still succeeds.

The combined test output includes all six shared metadata-containment cases (three per store):
missing metadata behind an external symlink, non-directory traversal, and replacement of an existing
valid metadata directory with an external symlink. Reads and writes fail without creating or changing
outside files, and existing valid external metadata remains byte-identical.

## Scope and provenance

These are portable orchestration, conformance and deterministic executable-fixture checks. No new
Android, Chromium or iOS device execution is claimed. WALK-017's graph and WALK-018's split are
derived only from accepted curated WALK-020 cold/warm evidence. WALK-016 remains a deliberately
narrow lexical proof; WALK-019 retains its documented single-writer, cross-resource commit and
hostile-race limitations; WALK-018 is not general route planning or subflow discovery.

Independent reviewers previously rejected the SQLite metadata-directory escape before its fix and
three WALK-018 assertion/verification/reference defects plus a later composed-output symlink bypass.
Their findings, reproduced adversarial checks and final verdicts are in
[p1-final-review.md](p1-final-review.md). Final independent verdicts: SQLite containment and route
graph **PASS**; WALK-018 after all corrections **PASS**, with no current in-scope blocker. The
remaining limits are documented rather than silently waived.
