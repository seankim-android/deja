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

`WalkCache` receives runner/store/reporter instances plus **lazy** explorer and repairer factories.
The model client belongs to the explorer implementation, so the explorer factory is also its
construction boundary. `run` performs `store.lookup` before either factory can be called. Only a
normal miss constructs the explorer; only eligible selector staleness constructs the repairer.
Product assertions, navigation/runner/timeout failures, and unknown failures stop without repair.
Core has no goal/app/platform checks and no Maestro/OpenClaw/React imports.

Concrete wiring lives at the edges: `store-local`, `maestro`, `agent-openclaw`, `repair`, and the
headless CLI's local JSON reporter. Their richer vendor-specific records are translated into the
shared result contract without weakening their original safety gates.
