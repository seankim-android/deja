# Bounded route graphs and screen fingerprints (WALK-017)

Route graphs are **diagnostic navigation memory**, not a second cache and not an executable flow.
They do not participate in `WalkCache.run`, `LocalWalkStore.lookup`, exploration, repair, or cache
identity. The existing exact replay and cold-exploration lifecycle remains the only way to execute or
persist a walk.

## Closed data contract

`packages/core/src/route-graph.ts` exports schema version 1, validation, fingerprint construction,
and diagnostic matching. A graph is bounded to 64 nodes, 256 verified transitions, 256 failed
branches, 16 observations per node, and 16 hashed provenance artifacts. Unknown fields and unsupported
versions are rejected rather than ignored.

A graph records only:

- exact Android `appId` and APK SHA-256 build identity;
- independently validated and exact-replay validation timestamps;
- nodes containing bounded, unique Android resource-ID observations and their recomputed SHA-256
  fingerprint;
- separately identified verified tap transitions and failed branches, each with source node, bounded
  action identifier, category/timestamp as applicable; and
- closed provenance: either a deterministic fixture ID or a ticket/run ID plus SHA-256 hashes of
  relative curated JSON/YAML/Markdown files. Provenance must explicitly say
  `newDeviceRunRecorded: false`.

Raw visible text, accessibility/UI trees, input values, credentials, personal data, screenshots,
video, arbitrary metadata, environment variables, device paths, and unrestricted artifacts have no
schema field and are rejected as unknown fields or unsupported artifact types. Resource IDs must be
individually curated stable identifiers; callers must not serialize arbitrary runtime identifiers.

Fingerprints canonicalize observation order before SHA-256 hashing. Supplying a fingerprint in a graph
does not establish trust: validation recomputes it from the bounded observations. A verified edge
attests that the complete source flow passed with that step; it omits input/precondition details and
is **not** independently replayable. In particular, a login edge never stores login values.

## Conservative matching

`matchScreen` always returns `cacheHit: false` and never calls a store:

| Status | Meaning | Fallback |
| --- | --- | --- |
| `matched` | Exactly one fingerprint matches and it is the expected node, if supplied | None; existing orchestration continues unchanged |
| `repeated` | The single match is also the previously matched node | None; this is repetition, not a cache hit |
| `ambiguous` | More than one node has the fingerprint, even if one was expected | `existing-exact-replay-or-exploration` |
| `changed` | No match for an expected node, or a different known node matches | `existing-exact-replay-or-exploration` |
| `unknown` | No match and no expected node was supplied | `existing-exact-replay-or-exploration` |

The fallback value directs a caller to the already supported orchestration path; the route module
does not launch replay, exploration, or repair itself. Changed, ambiguous, and unknown observations
therefore cannot weaken exact identity or manufacture a route-based hit. Duplicate node fingerprints
are accepted by the schema specifically so matching can report ambiguity instead of choosing one.
Duplicate logical nodes and duplicate successful transitions are invalid. Failed branches remain in
`failedBranches` and are never returned as verified transitions.

## Curated Android graph and exact provenance

[`walk017-route-graph.json`](../spikes/001-android-walkcache/evidence/walk017-route-graph.json) is a
four-node, three-transition Android graph for `dev.walkcache.checkout` at build SHA-256
`6aec5fea876938db14007ba3dbcb91f698832fe855886abd9dea1e7b28861759`:

```text
screen_login --login_button--> screen_catalog
screen_catalog --add_to_cart--> screen_cart
screen_cart --checkout_button--> screen_confirmation
```

It was **derived from existing curated WALK-020 evidence, not a newly recorded device run**. The
source run is `walk020-android-checkout-06`, fresh cold/warm lifecycle only. The deriver requires:

1. byte-identical [`cold-saved-checkout.yaml`](../demo/live-evidence/android/cold-saved-checkout.yaml)
   and [`warm-saved-checkout.yaml`](../demo/live-evidence/android/warm-saved-checkout.yaml), with flow
   SHA-256 `e95532ce04a4e125d3bd56e1b9d5d367b31dd67eed4441c56271ca52f3c3d0a1` and the exact accepted
   resource-ID/assertion/tap sequence;
2. a passing independently verified cold record in
   [`cold-stored-memory.json`](../demo/live-evidence/android/cold-stored-memory.json), whose
   `lastValidation.at` is `2026-11-22T20:27:33.668Z`;
3. matching app/platform/build/flow identity in
   [`cold-run-report.json`](../demo/live-evidence/android/cold-run-report.json), with outcome
   `cold-recorded`; and
4. a passing exact warm hit with zero model calls in
   [`warm-run-report.json`](../demo/live-evidence/android/warm-run-report.json), whose execution ended
   at `2026-11-22T20:28:37.591Z`.

The five source paths and their SHA-256 hashes are recorded in graph provenance. Only the four stable
`screen_*` root IDs become observations. Raw visible text, credential-entry steps, unrelated control
IDs, screenshots, recordings, emulator details, and raw logs are not copied into the graph. The accepted
fresh cold/warm evidence contains no failed navigation branch, so its curated `failedBranches` array
is intentionally empty; failed-branch separation is covered by deterministic fixtures rather than
fabricating an Android failure or mixing the separate WALK-020 repair fixture into this build graph.

Revalidate the source evidence and tracked graph without a device, model, credentials, or external
service:

```sh
bun scripts/derive-walk017-route-graph.ts --check
bun test packages/core/test/route-graph.test.ts packages/core/test/route-graph-evidence.test.ts --timeout=10000
```

The first command rehashes and rechecks all source facts, derives the graph deterministically, runs the
closed-schema validator, and compares serialized bytes with curated evidence. Tests cover ordering,
bounds, malformed/extra fields, screenshots/path escapes, broken references, timestamp/checksum
validation, failed-branch separation, repeated/ambiguous/changed/unknown states, and evidence-graph
re-derivation.

## Acceptance limits

This is a small evidence-backed navigation graph, not proof of complete app coverage, autonomous
composition, reusable login subflows, semantic retrieval, route-based replay, or current device
compatibility. WALK-018 remains a separate dependency. No emulator/simulator was used for WALK-017,
and no post-WALK-020 Android execution is claimed.
