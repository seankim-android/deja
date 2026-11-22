# Vendor-neutral WalkCache contracts and orchestration

`src/contracts.ts` defines goals, exact walk identity/storage records, run results, failures,
artifacts/diffs, exploration/verification metrics, and the plugin interfaces. It imports no runtime,
UI, framework, platform, or storage implementation. The interfaces are:

- `WalkStore`: exact lookup, prior-success diagnostic lookup, verified persistence/replacement,
  validation history, and listing.
- `WalkRunner`: execute a flow and return timing, assertions, artifacts, exit state, and conservative
  failure classification.
- `WalkExplorer`: cold exploration plus independently verified persistence.
- `SelectorRepairer`: one bounded repair result, including source diff and preservation evidence.
- `RunReporter`: serialize a completed run without influencing orchestration decisions.
- `GoalEmbeddingProvider`: lazy one-goal embeddings for the opt-in WALK-016 proof. Embedding calls
  are reported separately from exploration/model calls.

`WalkCache` receives runner/store/reporter instances plus **lazy** explorer and repairer factories.
The model client belongs to the explorer implementation, so the explorer factory is also its
construction boundary. `run` performs `store.lookup` before either factory can be called. Only a
normal miss constructs the explorer; only eligible selector staleness constructs the repairer.
Product assertions, navigation/runner/timeout failures, and unknown failures stop without repair.
`ExactFirstWalkRetriever` owns exact-first semantic fallback. It runs only on exact `not-found`,
rechecks each candidate through `WalkStore.lookup` before candidate embedding/ranking, rejects low
confidence/ambiguous/contradictory/narrow adversarial goals, and returns provider failures as ordinary
misses. A semantic hit still requires runner replay and records validation on the selected stored
identity. Core has no Maestro/OpenClaw/React imports.

`src/route-graph.ts` is a separate diagnostic-only contract for bounded Android navigation memory.
It validates stable curated resource-ID observations, recomputed screen fingerprints, app/build and
validation identity, verified transitions, separate failed branches, and hashed evidence provenance.
`matchScreen` conservatively distinguishes matched, repeated, ambiguous, changed, and unknown screens.
Every result has `cacheHit: false`; unsafe or unrecognized states name the existing exact
replay/exploration fallback. Neither orchestration nor `WalkStore` calls this module, so route graphs
cannot broaden cache identity or the P0 execution path. See
[`docs/ROUTE_GRAPH.md`](../../docs/ROUTE_GRAPH.md).

`src/subflow.ts` similarly remains outside `WalkCache.run`: it defines exact-identity reusable segment
contracts with typed input descriptors (not values), outputs, conditions, counts, and provenance.
`VerifiedSubflowStore` is not another goal cache and never does semantic selection. The Maestro edge
owns independent segment verification, explicit reference-based composition, and complete-journey
verification before goal persistence. See
[`docs/SUBFLOW_COMPOSITION.md`](../../docs/SUBFLOW_COMPOSITION.md).

Concrete wiring lives at the edges: `store-local`, `store-sqlite`, `maestro`, `agent-openclaw`,
`repair`, and the headless CLI's local JSON reporter. Their richer vendor-specific records are
translated into the shared result contract without weakening their original safety gates. WALK-019
runs one unchanged `WalkStore` behavior suite against both real stores plus one production-runner
warm orchestration per store; see `packages/store-conformance`.
