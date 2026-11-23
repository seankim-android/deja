> Historical build log from the Cerebras x Kimi hackathon (Nov 21 to 22, 2026).
> The project was called WalkCache then.

# WalkCache project plan

**Status:** planned on November 21, 2026  
**Submission deadline:** November 22, 2026 at 10:00 AM PT  
**Tagline:** Explore once. Replay instantly. Repair only when needed.

## Product thesis

WalkCache is a learning layer around Maestro, not another agent that merely writes tests.
On a cold run, an OpenClaw agent using Kimi K3.1 Pro on Cerebras explores an app and saves
a verified, independently runnable Maestro flow. On a warm run, WalkCache executes that flow
directly with zero model calls. If a selector becomes stale, the agent repairs the walk or
proposes a targeted source selector patch and verifies it.

The demo must prove three things with observable evidence:

1. A cold run explores, verifies, and records a walk.
2. A warm run replays it deterministically with zero model calls and less agent time.
3. A stale walk triggers bounded repair rather than blind exploration or a falsely passing test.

## MVP contract

The primary journey is **login -> add item -> checkout**. The feasibility PoC comes first in a
dependency-free native Android app under `spikes/001-android-deja`, per Sean's Android-first
direction. Expo/React Native is deferred until the Android cold/warm loop is validated. The future
architecture supports iOS, Android, and web through platform adapters; later platforms must not
slow down or weaken the first working Android proof.

### Required behavior

- Accept a natural-language QA goal plus app and platform configuration.
- Look up a matching successful walk before invoking a model.
- Explore through Maestro on a cold run and record actions plus meaningful assertions.
- Persist memory only after the complete flow passes.
- Replay successful YAML directly through `maestro test` on a warm run.
- Report wall-clock time, model calls, cache status, steps, assertions, and repair count.
- Detect stale selectors and perform one bounded repair attempt.
- Patch only configured source roots; show the patch and never auto-commit it.
- Invalidate a stale cached walk atomically after the replacement passes.
- Keep generated YAML runnable without WalkCache or an LLM in CI.

### Correctness and safety rules

- A meaningful product assertion failure is a potential regression, not a selector problem.
  Report it and do not rewrite the assertion to make the product pass.
- Failed exploration attempts go to attempt logs, never into successful reusable flows.
- Do not add selectors to every element. Patch only the component needed for a stable walk.
- Never record credentials or secrets in committed flows, logs, screenshots, or reports.
- Keep source writes inside explicitly configured demo workspaces for the hackathon.
- Use a conservative cache key: normalized goal, app, platform, flow schema, and build fingerprint.
- Treat platform support as verified only after a real run; do not infer it from shared YAML.

## Repository architecture

Use Bun workspaces and TypeScript. Keep core contracts independent of OpenClaw, Maestro,
and any particular UI framework so the project can become an open-source SDK later.

```text
apps/
  cli/                 Command-line entry point and human-readable reports
  dashboard/           Existing Vite app, repurposed after the CLI works
packages/
  core/                Goal, walk, run result, errors, and orchestration contracts
  maestro/             Maestro runner and flow serialization adapter
  agent-openclaw/      OpenClaw + Kimi exploration through Maestro MCP
  store-local/         Atomic filesystem walk memory and attempt logs
  repair/              Bounded selector diagnosis and source-patch interface
spikes/
  001-android-deja/ Minimal native Android feasibility proof
examples/
  checkout-demo/       Later Expo app targeting iOS, Android, and web
  flows/               Small checked-in, secret-free example Maestro flows
docs/
  PROJECT_PLAN.md      Scope, architecture, milestones, and demo plan
  ARCHITECTURE.md      Plugin contracts and memory schema after the vertical slice
```

This is the target layout, not a prerequisite big-bang rewrite. Create packages as the first
working vertical slice needs them. Avoid spending the first hour on workspace configuration.

### Initial plugin boundaries

- `WalkStore`: retrieval, successful-flow persistence, attempt logs, and invalidation.
- `WalkRunner`: platform-aware Maestro execution and structured results.
- `WalkExplorer`: cold-path exploration and verified flow generation.
- `SelectorRepairer`: bounded diagnosis and minimally scoped source changes.
- `RunReporter`: machine-readable metrics and terminal/dashboard rendering.

The default implementation is local filesystem storage + Maestro + OpenClaw/Kimi. Alternative
stores, runners, explorers, and reporters must not require changes to core orchestration.

## Memory format

Generated memory belongs in an ignored `.walkcache/` directory by default:

```text
.walkcache/
  flows/<goal>/<platform>-<flow-sha256>.yaml
  memory/walks.json
  attempts.jsonl
  reports/latest.json
  patches/                 Proposed source diffs and verification evidence
```

Each successful walk records its identity, app/build fingerprint, flow schema/checksum, assertions,
selector/source-patch history, timings, model-call count, last validation result, and validation
history. Memory writes are atomic: a failed replacement never destroys the last known-good walk, and
a failed latest validation is never reused as a successful hit.

## Time-boxed milestones

Times are stopping points, not aspirations. If a milestone misses its exit gate, cut its polish
and protect the next demonstration-critical milestone. The actionable checklist is in `TODO.md`;
repo-local ticket scope, dependencies, acceptance criteria, and evidence live in `tickets/`.

### M0 — Plan and Android environment gate — completed

- Lock this scope and repository boundaries, then switch the first proof to Android per Sean.
- Locate Maestro and install Android platform/command-line tools plus an API 34 emulator.
- Verify the emulator through Maestro before building orchestration around it.
- Defer web/iOS environment work to M5.
- **Exit gate:** the Android execution mechanism is available; MCP actions are checked in M2.

### M1 — Android demo app and manual Maestro smoke — completed

- Build a dependency-free native Java checkout journey with deterministic test data and assertions.
- Add stable Android resource IDs everywhere except checkout, which is reserved for repair.
- Write and run one hand-authored Maestro YAML flow on Android.
- Keep this feasibility spike separate from the future Expo and production package structure.
- **Exit gate:** the 15-step journey passes through Maestro without any agent involvement.

### M2 — Cold exploration vertical slice — by 10:00 PM

- Connect OpenClaw/Kimi to Maestro through the verified MCP/CLI path.
- Accept a goal, explore the demo, and capture a candidate flow with assertions.
- Save the flow only after a clean replay passes.
- Log failed branches separately and count model calls.
- **Exit gate:** one cold goal produces independently runnable YAML and a structured report.

### M3 — Persistent memory and warm replay — by 11:45 PM

- Implement exact cache lookup and filesystem storage.
- Execute a cache hit directly with no model invocation.
- Instrument cold/warm wall time, model calls, cache status, and result.
- Add unit tests for retrieval, malformed memory, and atomic persistence.
- **Exit gate:** the second identical run proves `modelCalls: 0` and passes from saved YAML.

### M4 — Invalidation and targeted selector repair — by 1:30 AM

- Add a controlled UI change that makes the checkout selector stale.
- Distinguish selector/observation failures from meaningful product assertion failures.
- Generate one minimal selector/source patch, rebuild, and rerun within configured roots.
- Replace cached memory only after verification; preserve the original as an attempt artifact.
- **Exit gate:** the repair demo passes, and a simulated product regression remains a failure.

### M5 — Cross-platform proof (after the Android PoC)

- Complete Android evidence first, then run the same goal through adapters on web and iOS.
- Use `testID` on native and the verified stable DOM selector mechanism on web.
- Record an honest support matrix with actual command evidence and platform limitations.
- **Exit gate:** all three target platforms run, or a concrete tool blocker is documented and the
  demo narrative uses only the verified platforms. Do not fake the matrix.

### M6 — Demonstrable CLI and dashboard — by 4:30 AM

- Polish one CLI command for `run`, including clear cold/warm/repair status.
- Show saved YAML, run metrics, and source diffs in the existing Vite dashboard.
- Keep the dashboard a viewer/controller; core orchestration must remain usable headlessly.
- **Exit gate:** another person can understand the entire story without opening source code.

### M7 — OSS readiness and rehearsal — by 6:30 AM

- Add Apache-2.0 licensing, package metadata, README quick start, architecture, and limitations.
- Document secret handling, source-write boundaries, cache invalidation, and Maestro web beta status.
- Run lint, type-check, unit tests, production builds, and all supported demo flows from clean state.
- Rehearse the 60-second recording twice and keep a deterministic prerecorded fallback.
- **Exit gate:** a clean checkout can reproduce the documented demo and CI is green.

### M8 — Video and submission — by 9:00 AM

- Record the 60-second video with large readable text and no credential exposure.
- Show the cold/warm metrics, one YAML artifact, and one verified selector repair.
- Post on X with `@Cerebras`, `@Kimi_Moonshot`, and `#KimiOnCerebras` per the event requirements.
- Complete the official submission with repository and video links.
- **Exit gate:** submission is confirmed by 9:00 AM, leaving one hour for upload failures.

## The 60-second demo

1. **0–5s:** “WalkCache learns QA walks. Explore once; replay without an agent.”
2. **5–18s:** Start a cold checkout goal. Show exploration and a missing-selector repair.
3. **18–25s:** Show the saved Maestro YAML and passing assertion.
4. **25–40s:** Rerun the same goal. Emphasize cache hit, zero model calls, and elapsed-time delta.
5. **40–50s:** Change the UI, show stale-memory detection and bounded verified repair.
6. **50–60s:** Show the platform matrix and pluggable SDK boundaries; end on the tagline.

Do not rely on live model or simulator luck for the recorded submission. Capture a reproducible
run with terminal/dashboard output and retain raw metrics behind every claim.

## Cut line

Cut in this order when behind schedule:

1. Dashboard animations, branding, and extra screens.
2. Semantic retrieval, route graphs, subflow composition, and multiple repair attempts.
3. Additional demo journeys and alternate storage providers.
4. Native-platform polish before platform plumbing; never replace verified results with claims.

Never cut the cold/warm comparison, meaningful assertions, zero-model instrumentation, or the
distinction between a product regression and a stale selector.

## Post-hackathon roadmap (not MVP work)

- Reusable login/navigation subflows and route-graph composition.
- Semantic goal retrieval with deterministic exact-match fallback.
- Playwright/Appium adapters, remote stores, CI services, and parallel sharding.
- Rich trace viewer, quarantine/flake analytics, and reviewed pull-request creation.
- Signed releases, compatibility fixtures, plugin conformance tests, and contribution guides.

