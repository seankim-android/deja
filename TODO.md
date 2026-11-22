# WalkCache TODO

The ordered execution checklist for `docs/PROJECT_PLAN.md`. Times are Pacific on the hackathon
weekend. Check an item only when its evidence exists; keep unsupported work unchecked.

**P0** = required for the core demo. **P1** = polish after all P0 gates pass.

## M0 — Plan and environment gate (due 5:45 PM)

- [x] Create the initial React + TypeScript + Bun repository with CI.
- [x] Write the project plan, milestone gates, and cut line.
- [ ] Install or locate Maestro and record its version.
- [ ] Verify Maestro MCP can inspect and operate a target app.
- [ ] Verify Maestro web automation against Chromium and record current limitations.
- [ ] Verify an available iOS simulator/runtime.
- [ ] Locate the Android SDK, emulator, and `adb`; fix PATH if needed.
- [ ] Run one trivial Maestro command and retain its output.
- [ ] **Gate:** demonstrate the actual execution path before integrating it.

## M1 — Demo app and manual Maestro smoke (due 7:30 PM)

- [ ] Scaffold `examples/checkout-demo` with Expo/React Native.
- [ ] Implement deterministic login, catalog, cart, and checkout screens.
- [ ] Use test credentials and fixture data; add no real payment or personal data.
- [ ] Add stable native/web selectors through a small platform helper.
- [ ] Deliberately omit only the checkout selector needed for the repair demonstration.
- [ ] Add a visible final order confirmation and a meaningful Maestro assertion.
- [ ] Hand-write the reference login -> add item -> checkout YAML flow.
- [ ] Pass that flow on web without invoking an agent.
- [ ] Ignore generated `.walkcache/` artifacts while allowing curated example flows.
- [ ] **Gate:** retain the passing manual flow and command as the baseline.

## M2 — Cold exploration vertical slice (due 10:00 PM)

- [ ] Add the minimal Bun workspace/package structure required by the working slice.
- [ ] Define core goal, walk metadata, attempt, run result, and error types.
- [ ] Implement the verified Maestro runner adapter with timeouts and structured failures.
- [ ] Connect the OpenClaw/Kimi explorer to Maestro MCP or the verified equivalent.
- [ ] Accept a goal from the CLI and drive the demo without a prewritten target flow.
- [ ] Record successful actions as valid Maestro YAML with meaningful assertions.
- [ ] Log failed branches and attempts separately from successful walks.
- [ ] Replay the candidate flow in a fresh session before saving it.
- [ ] Track wall-clock duration and model-call count in a JSON report.
- [ ] Redact environment values and test secrets from logs/reports.
- [ ] **Gate:** cold exploration produces YAML that passes independently via `maestro test`.

## M3 — Persistent memory and warm replay (due 11:45 PM)

- [ ] Implement conservative walk lookup by goal, app, platform, schema, and build fingerprint.
- [ ] Add atomic filesystem writes and corrupted-memory handling.
- [ ] Save successful flows and metadata under `.walkcache/`.
- [ ] Skip all explorer/model calls on a valid cache hit.
- [ ] Run saved YAML directly and emit explicit `cacheStatus` and `modelCalls` fields.
- [ ] Update validation timestamps/timings without discarding history.
- [ ] Test cache miss, hit, stale build, malformed metadata, and interrupted writes.
- [ ] Capture side-by-side cold and warm metrics for the demo.
- [ ] **Gate:** prove a passing second run with exactly zero model calls.

## M4 — Invalidation and targeted selector repair (due 1:30 AM)

- [ ] Create a repeatable fixture change that invalidates the checkout selector.
- [ ] Classify selector/navigation failure separately from final product assertion failure.
- [ ] Bound repair to one attempt, a time limit, and explicitly configured source roots.
- [ ] Generate the minimal native/web selector patch only for the affected component.
- [ ] Display a diff and verification result; do not auto-commit source changes.
- [ ] Rebuild and rerun the repaired walk before replacing cached memory.
- [ ] Preserve the old flow and failed replacement as attempt artifacts.
- [ ] Add a regression fixture proving a genuine product failure is not “repaired” into a pass.
- [ ] **Gate:** demonstrate stale-selector recovery and correct regression reporting.

## M5 — Cross-platform proof (due 3:00 AM)

- [ ] Run and record the core journey on web through its adapter.
- [ ] Build and launch the demo on an iOS simulator; run its Maestro flow.
- [ ] Build and launch the demo on an Android emulator; run its Maestro flow.
- [ ] Verify platform selector adapters produce selectors Maestro actually resolves.
- [ ] Add platform/build identity to cache keys and reports.
- [ ] Document the verified platform matrix, exact commands, and any blockers.
- [ ] Fix shared orchestration only through platform interfaces, not per-demo conditionals.
- [ ] **Gate:** real evidence for every claimed platform; remove any unverified demo claims.

## M6 — Demonstrable CLI and dashboard (due 4:30 AM)

- [ ] Provide one reliable `walkcache run <goal>` entry point with useful help text.
- [ ] Render cold exploration, warm cache hit, failure, and repair states legibly.
- [ ] Add commands to list walks and inspect a run report.
- [ ] Repurpose the Vite starter as a simple local WalkCache viewer/controller.
- [ ] Show elapsed time, model calls, cache status, assertions, and repair count.
- [ ] Link/display generated YAML and proposed source diffs without exposing secrets.
- [ ] Preserve full headless CLI behavior when the dashboard is unavailable.
- [ ] **Gate:** a fresh viewer can understand cold vs warm vs repair without reading source.

## M7 — OSS readiness and rehearsal (due 6:30 AM)

- [ ] Add Apache-2.0 license and accurate package/repository metadata.
- [ ] Rewrite README around WalkCache with a five-minute quick start.
- [ ] Document plugin boundaries, memory schema, security boundaries, and limitations.
- [ ] Document Maestro web's current beta/Chromium constraints based on verified behavior.
- [ ] Add focused unit tests for core orchestration, storage, and failure classification.
- [ ] Run lint, TypeScript checks, tests, builds, and supported Maestro flows from clean state.
- [ ] Ensure GitHub Actions exercises all portable checks and passes.
- [ ] Remove dead starter content, screenshots with secrets, and accidental generated artifacts.
- [ ] Record a deterministic fallback demo and raw metric evidence.
- [ ] Rehearse the 60-second script twice and verify every spoken claim.
- [ ] **Gate:** clean-checkout reproduction works, checks pass, and the recording is reliable.

## M8 — Video and submission (due 9:00 AM)

- [ ] Record the final video at 60 seconds or less with readable output.
- [ ] Include cold exploration, saved YAML, zero-model warm replay, and targeted repair.
- [ ] Show only verified platform support and actual metrics.
- [ ] Review every frame for credentials, personal data, and local filesystem secrets.
- [ ] Post on X tagging `@Cerebras` and `@Kimi_Moonshot` with `#KimiOnCerebras`.
- [ ] Submit the repository and video through the official event process.
- [ ] Confirm submission/upload completion no later than 9:00 AM.
- [ ] Keep 9:00–10:00 AM exclusively for upload or submission failures.

## P1 — Only after every relevant P0 gate is green

- [ ] Reuse a verified login subflow in a different checkout goal.
- [ ] Add exact-match-first semantic goal retrieval.
- [ ] Add screen-state fingerprints and a minimal route graph.
- [ ] Add a second storage implementation to prove plugin independence.
- [ ] Add package contribution guide, code of conduct, issue templates, and release workflow.

## Explicitly not in the hackathon MVP

- [ ] No hosted multi-tenant service or account system.
- [ ] No autonomous pull-request creation or unrestricted source edits.
- [ ] No broad selector generation across an entire app.
- [ ] No remote vector database when exact local lookup is sufficient.
- [ ] No unsupported claim that shared Maestro YAML proves every platform works.

