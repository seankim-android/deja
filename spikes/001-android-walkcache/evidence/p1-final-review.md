# P1 independent final review

Review target for the initial combined review: `79c2c3b9caf09fee3ec599fab591749000a30f6e`.
No real device execution or new secrets were used by either reviewer.

## SQLite containment + route graph: PASS

The original reviewer reran the external `.walkcache/memory` symlink reproduction against the fixed
integration. Persistence failed with `Invalid walk memory: metadata path escapes workspace`; the
external directory remained empty and no `walks.sqlite` was created. Canonical nearest-existing
ancestor checks, cached/writable connection rechecks, and equivalent JSON-store protection were
inspected. All three shared containment regressions passed for both stores, including a valid cached
store whose metadata directory was replaced by an external symlink. Existing metadata remained
byte-identical and no SQLite sidecars appeared. The documented hostile check/open symlink-race limit
remains honestly disclosed and is not claimed solved.

The route-graph reviewer found no false-cache/privacy blocker: every `matchScreen` result remains
`cacheHit: false`; changed/ambiguous/unknown states use conservative fallback; route matching does
not call `WalkStore` or orchestration; observations and provenance are bounded. Focused verification:
74 tests / 246 assertions, plus successful route-graph re-derivation/schema checks.

## WALK-018 initial review: NOT-PASS — findings being fixed

1. The deterministic Maestro composition fixture checked only structured `assertVisible.id`, ignored
   `text`, and set confirmation success from the screen alone. The reviewer replaced the exact order
   and item strings with wrong values and the runner still claimed 14 passing assertions and a final
   product assertion. This made the exact final-text evidence unsupported.
2. Segment verification accepted a second `clearState` launch after the catalog postcondition. It
   persisted the segment even though the final screen returned to login. Clean-launch count/order,
   screen preconditions, and postconditions at completion must be enforced from actual execution.
3. The exported composition helper rejected safe same-directory `login.yaml` references but accepted
   `../../../outside.yaml`. Proper same-directory acceptance and escape rejection are required.

Existing use of one `runFlow`, 11 reused + 11 new steps, syntactic assertion retention, exact replay,
zero model/explorer construction, failure preservation, and reference-cycle rejection otherwise
passed. The reviewer also reran the full combined suite successfully; the older curated log remains
historical evidence rather than an exact-HEAD transcript.

Corrected integrated target: `ccee2a154f074119642b53ce38ef50ad053fe135` (author fix `199734f`).
Structured assertions now enforce ID and exact text; observed runner traces enforce one first-step
clean launch, login precondition and trailing catalog postcondition; final reports derive the exact
product assertions from observations; same-directory references are contained without rejecting
safe `login.yaml`. The corrected suite passes 221 tests / 1,107 assertions, lint/build, and both
deriver/verifier checks. The wrong-text and late-launch probes now fail closed with byte-identical
last-known-good storage.

Second independent review at `ccee2a154f074119642b53ce38ef50ad053fe135` confirmed wrong-text,
late-launch and lexical-traversal attacks now fail safely, and reproduced their byte-preservation.
It returned NOT-PASS for one remaining bypass: replacing the composed goal directory with an
external symlink still let `compose()` create/overwrite an external `candidate.yaml` before runner
rejection. Canonical workspace and nearest-existing-parent checks are now required before every
composed-output `mkdir`/write/rename, with regression proof that no outside file changes.

Canonical-containment target: `e7252d8994b30fae54e07f09f8d9966ae7727281` (author fix `02860755`).
Five filesystem variants now reject during output preflight with zero runner calls: goal-directory
symlink, composed-root symlink, dangling goal symlink, and non-directory goal/root traversal.
Existing external sentinels and all goal/segment snapshots remain byte-identical; legitimate nested
output still succeeds. The integrated suite passes 221 tests / 1,129 assertions, lint/build, and all
deriver/verifier checks. Hostile concurrent path-component replacement remains an explicit narrow
limit, not a claim of race-proof filesystem access.

## Final WALK-018 verdict: PASS

The independent reviewer reproduced the original goal-directory symlink attack at
`e7252d8994b30fae54e07f09f8d9966ae7727281`: preflight rejected the symbolic link with zero runner
calls; the external sentinel, segment index/YAML and goal storage remained unchanged. An equivalent
already-existing candidate symlink was also rejected with zero runner calls and no changes. The
legitimate nested-output control composed and persisted successfully with one runner call.

The reviewer confirmed canonical nearest-existing-parent validation before mutation, post-creation
revalidation, non-directory/dangling-link and regular-file checks, and exclusive temporary-file
creation. Deriver/verifier checks passed; focused verification passed 42 tests / 259 assertions; the
full combined suite passed 221 tests / 1,129 assertions. No current in-scope blocker remains.
Concurrent hostile path replacement is still explicitly outside the narrow preexisting-filesystem
threat claim; no emulator or new real-device execution was used.

## Release gate

SQLite containment: **PASS**. Route graph: **PASS**. Semantic/store integration: no additional
blocking finding. WALK-018 after all corrections: **PASS**. The portable P1 integration is approved
for push by the recorded independent reviews, with the evidence and limitations above.
