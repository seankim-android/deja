# WalkCache TODO

The ordered execution checklist for `docs/PROJECT_PLAN.md`. Times are Pacific on the hackathon
weekend. Check an item only when its evidence exists; keep unsupported work unchecked.

**P0** = required for the core demo. **P1** = polish after all P0 gates pass.

## M0 — Plan and Android environment gate (completed 6:20 PM)

**Chunks:** **0A** Android toolchain → **0B** dedicated emulator → **0C** Maestro device discovery.
Web/iOS environment checks are deferred to M5 rather than blocking the Android proof.

- [x] Create the initial React + TypeScript + Bun repository with CI.
- [x] Write the project plan, milestone gates, and cut line.
- [x] Locate Maestro CLI 2.12.0 at `~/.maestro/bin/maestro` and expose it through the task PATH.
- [x] Install Android command-line tools, platform tools/`adb`, API 34, build tools, and emulator.
- [x] Create and boot dedicated `walkcache_api_34` Android 14 emulator.
- [x] List the booted Android emulator through Maestro 2.12.0.
- [x] **Gate:** the Android execution mechanism is available; MCP/tool execution is checked in M2A.

## M1 — Android demo app and manual Maestro smoke (next)

**Chunks:** **1A** dependency-free native fixture → **1B** Gradle build/install → **1C** manual Maestro baseline.
The PoC lives at `spikes/001-android-walkcache`; Expo and production packages wait for a verdict.

- [x] Scaffold a minimal native Java Android fixture with no runtime dependencies.
- [x] Implement deterministic login, catalog, cart, and checkout screens.
- [x] Use test credentials and fixture data; add no real payment or personal data.
- [x] Add stable Android resource IDs through `res/values/ids.xml`.
- [x] Deliberately omit only the checkout selector needed for the repair demonstration.
- [x] Add a visible final order confirmation and a meaningful Maestro assertion.
- [x] Hand-write the reference login -> add item -> checkout YAML flow.
- [x] Build and install the debug APK on `walkcache_api_34`.
- [x] Pass the 15-step reference flow on Android without invoking an agent.
- [x] Ignore generated `.walkcache/` artifacts while allowing curated example flows.
- [x] **Gate:** retain the passing manual flow and command as the baseline.

## M2 — Cold exploration vertical slice

**Chunks:** **2A** verify Maestro MCP transport/actions → **2B** let Kimi explore without the reference flow → **2C** independently replay its generated flow and save evidence.

- [x] Keep the first scripts inside the Android spike; defer production Bun workspaces/packages.
- [x] Define minimal goal, walk metadata, attempt, and run result types for the spike.
- [ ] Implement the verified Maestro runner adapter with timeouts and structured failures.
- [x] Connect the OpenClaw/Kimi explorer to Maestro MCP through `scripts/mcp-call.ts`.
- [ ] Accept a goal from the CLI and drive the demo without a prewritten target flow.
- [x] Record successful actions as valid Maestro YAML with meaningful assertions.
- [x] Log MCP calls and authoring failures separately from successful walks.
- [x] Replay the 21-command candidate from a fresh `clearState` launch before saving it.
- [ ] Track wall-clock duration and model-call count in a JSON report.
- [x] Keep environment dumps out of reports; use only documented fixture credentials.
- [x] **Gate:** cold exploration produces YAML that passes independently via `maestro test`.

## M3 — Persistent memory and warm replay

**Chunks:** **3A** minimal atomic local store → **3B** cache-first CLI replay with zero model calls → **3C** corruption/staleness tests and cold/warm metrics.

- [x] Implement conservative walk lookup by goal, app, platform, schema, and build fingerprint.
- [x] Add atomic filesystem writes and corrupted-memory handling.
- [x] Save successful flows and metadata under `.walkcache/`.
- [x] Skip all explorer/model calls on a valid cache hit.
- [x] Run saved YAML directly and emit explicit `cacheStatus` and `modelCalls` fields.
- [x] Update validation timestamps/timings without discarding history.
- [ ] Test cache miss, hit, stale build, malformed metadata, and interrupted writes.
- [ ] Capture side-by-side cold and warm metrics for the demo.
- [x] **Gate:** prove a passing second run with exactly zero model calls (24.824s measured).

## M4 — Invalidation and targeted selector repair

**Chunks:** **4A** classify selector failure vs product regression → **4B** one minimal source patch and rebuild → **4C** verified cache replacement and negative regression test.

- [ ] Create a repeatable fixture change that invalidates the checkout selector.
- [ ] Classify selector/navigation failure separately from final product assertion failure.
- [ ] Bound repair to one attempt, a time limit, and explicitly configured source roots.
- [x] Generate the minimal Android selector patch only for the affected Checkout component.
- [x] Display a diff and verification result; do not auto-commit during repair.
- [x] Rebuild and rerun the repaired walk before replacing cached memory.
- [x] Preserve the old flow and failed replacement as attempt artifacts.
- [ ] Add a regression fixture proving a genuine product failure is not “repaired” into a pass.
- [x] Demonstrate stale-selector recovery; keep the full gate open for regression reporting.

## M5 — Cross-platform proof (after Android PoC)

**Chunks:** **5A** complete Android evidence from M1–M4 → **5B** web adapter and Chromium proof → **5C** iOS adapter and simulator proof. Borrow Blankaday's ID-based Maestro conventions, not product-specific flows.

- [ ] Run and record the core journey on web through its adapter.
- [ ] Build and launch the demo on an iOS simulator; run its Maestro flow.
- [ ] Build and launch the demo on an Android emulator; run its Maestro flow.
- [ ] Verify platform selector adapters produce selectors Maestro actually resolves.
- [ ] Add platform/build identity to cache keys and reports.
- [ ] Document the verified platform matrix, exact commands, and any blockers.
- [ ] Fix shared orchestration only through platform interfaces, not per-demo conditionals.
- [ ] **Gate:** real evidence for every claimed platform; remove any unverified demo claims.

## M6 — Demonstrable CLI and dashboard

**Chunks:** **6A** reliable headless CLI → **6B** reports/artifact inspection → **6C** optional Vite viewer only after 6A/6B.

- [ ] Provide one reliable `walkcache run <goal>` entry point with useful help text.
- [ ] Render cold exploration, warm cache hit, failure, and repair states legibly.
- [ ] Add commands to list walks and inspect a run report.
- [ ] Repurpose the Vite starter as a simple local WalkCache viewer/controller.
- [ ] Show elapsed time, model calls, cache status, assertions, and repair count.
- [ ] Link/display generated YAML and proposed source diffs without exposing secrets.
- [ ] Preserve full headless CLI behavior when the dashboard is unavailable.
- [ ] **Gate:** a fresh viewer can understand cold vs warm vs repair without reading source.

## M7 — OSS readiness and rehearsal

**Chunks:** **7A** tests/clean-checkout evidence → **7B** architecture/security/package docs → **7C** deterministic recording and two rehearsals.

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

## M8 — Video and submission

**Chunks:** **8A** capture/review 60-second demo → **8B** publish required social post → **8C** confirm official submission before the buffer.

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

