# Déjà reference

Long reference material moved from the README.

## Verified support and limitations

| Platform | Verified behavior | Not supported or not claimed |
| --- | --- | --- |
| Android emulator, API 34, Maestro 2.12.0 | Cold exploration, independent verification/`remember`, exact warm replay, build invalidation, one bounded Checkout selector repair, and genuine product-regression handling | General-purpose repair, other apps/selectors without a new policy, physical devices, or other Android versions |
| Chromium Desktop via Maestro Web 2.12.0 **Beta/Experimental** | Independently reviewed YAML via `remember`, exact warm replay, static-build invalidation, and product-regression handling on the loopback fixture | **Cold exploration and repair are unsupported.** Firefox, Safari, mobile web, non-loopback targets, and parallel sharding are not claimed |
| iPhone 17 simulator, iOS 27.0, Xcode 27.2, Maestro 2.12.0 | Independently reviewed YAML via `remember`, exact warm replay, native `.app` build invalidation, and product-regression handling | **Cold exploration and repair are unsupported.** Physical devices, other runtimes/models, and other Maestro versions are not claimed |

Shared Maestro YAML does not establish cross-platform support. See the
[Android evidence](../spikes/001-android-deja/README.md),
[Chromium fixture and command boundary](../spikes/002-web-deja/README.md),
[native iOS fixture and command boundary](../spikes/003-ios-deja/README.md), and
the WALK-010/WALK-011 evidence files in those folders for the evidence behind this matrix. The
post-WALK-011 Android compatibility attempt was blocked before boot by insufficient emulator disk
space ([its record](../spikes/003-ios-deja/evidence/walk011-android-compatibility.txt)).

**No measured wall-clock speedup is claimed for the current CLI.** Warm replay eliminates model
calls; it does not make Maestro itself instant. Historical WALK-005 spike samples measured complete
cold commands at 96.5–97.0 seconds and warm commands at 24.3–25.9 seconds, while nested Maestro
runner ranges overlapped. Those historical spike measurements are not a current end-to-end CLI
benchmark or a performance guarantee. Current reports record per-run durations and metric scope;
compare like-for-like lifecycles before drawing timing conclusions. See
[the historical comparison](../spikes/001-android-deja/evidence/walk005-comparison.md).

## CLI

From a configured fixture directory, use `bun ../../apps/cli/deja.ts …`; from the repository
root, use `bun run deja -- … --config <path>`. Help works without configuration:

```sh
bun run deja -- --help
```

| Command | Behavior |
| --- | --- |
| `explore <goal>` | Force Android cold exploration; independently verify and persist only a passing candidate. Web and iOS return `exploration-rejected` with zero model calls. |
| `run <goal>` | Compute exact identity and check memory first. A hit replays YAML; a normal miss explores; eligible Android stale-build memory enters one bounded repair. A failed hit is recorded and is not reusable. |
| `remember <goal> --flow <candidate.yaml>` | Independently execute reviewed YAML and persist only after a complete pass. A bad product assertion remains a failure. |
| `list` | List valid local walks; does not invoke a runner, explorer, or model. |
| `inspect <report>` | Print a saved, redacted report. Inspecting a failed result returns its original failure class. |

Global options are `--config <file>` (default `./deja.config.json`), `--json`, and
`-h`/`--help`. `--flow` is valid only with `remember`. Goals may contain multiple words; quote them
as one shell argument.

### Configuration

The checked-in [Android config](../spikes/001-android-deja/deja.config.json) and
[Chromium config](../spikes/002-web-deja/deja.config.json) are complete fixture examples.
Paths are resolved from the config/workspace as described below; use separate configs for separate
workspaces.

| Field | Meaning |
| --- | --- |
| `workspaceRoot` | Canonical workspace boundary, relative to the config file. `.deja/` lives here. |
| `appId`, `platform`, `buildPath`, `device` | App/platform identity, APK or static tree to fingerprint, and execution target. |
| `openClawModel`, `openClawBaseUrl`, `explorationTimeoutSeconds` | Android cold-exploration model/provider and bound. |
| `verificationTimeoutMs`, `maestro.timeoutMs` | Independent verification and Maestro execution limits in milliseconds. |
| `fixtureCredentials` | Explicit deterministic fixture values only; never real user credentials. Values are redacted from CLI output/reports. |
| `maestro.executable`, `openClaw.executable`, `openClaw.mcpProxy` | Executable names and the workspace-local Android MCP proxy. |
| `repair.sourceRoots`, `repair.permittedFileTypes`, `repair.affectedFiles`, `repair.timeBudgetMs` | Closed Android repair allowlist and one shared deadline. No repair block is permitted for Web. |
| Optional `semantic.provider`, `minimumSimilarity`, `minimumMargin` | WALK-016 opt-in local retrieval proof. `local-lexical-v1` is the only implementation; defaults are 0.90 similarity and 0.15 winner margin. Missing/unsupported providers report unavailable and use the ordinary miss lifecycle. |
| Web-only `adapter`, `browser`, `headless`, `screenSize` | Exact execution/cache identity and Chromium launch settings. Only loopback `appId` URLs and `browser: chromium` are accepted. |
| iOS `adapter`, `.app` `buildPath`, simulator-UDID `device` | Exact native bundle-tree/adapter identity and explicit `--platform ios` execution on the dedicated simulator. No repair block is permitted. |

When a corresponding JSON executable/timeout field is omitted, `DEJA_MAESTRO`,
`DEJA_OPENCLAW`, and `DEJA_TIMEOUT_MS` supply defaults. `ANDROID_HOME`/
`ANDROID_SDK_ROOT` and `JAVA_HOME` come from the environment and are not serialized into reports.
The spike also recognizes its more specific repair override; the production config's documented
budget is `repair.timeBudgetMs`.

### Opt-in exact-first semantic proof (WALK-016)

Semantic retrieval is off by default. Enable the deterministic local proof in a fixture config:

```json
{
  "semantic": {
    "provider": "local-lexical-v1",
    "minimumSimilarity": 0.9,
    "minimumMargin": 0.15
  }
}
```

Exact identity is always checked first and never calls or constructs an embedding provider. Only an
exact `not-found` miss enters this fallback. Candidate walks must still match app, platform, schema,
build, adapter/browser, canonical flow checksum, and latest passed validation through the local store
before they are ranked. Equivalent goals can then select one walk only above both similarity/margin
thresholds; unrelated, low-confidence, ambiguous, contradictory, and narrow adversarial-instruction
goals do not select a walk. Adversarial markers stop before cold exploration. A missing provider or
embedding failure instead follows the ordinary verified miss/explore lifecycle.
Both floors may only be raised from the defaults, never lowered/disabled. The local provider also
requires canonical-token equality after scoring so a raw hash collision cannot become a hit.

Every selected flow is revalidated immediately after provider calls and then executed by Maestro;
its result is recorded against the original stored exact identity. Semantic selection never
substitutes for verification and never broadens
repair eligibility. Human/JSON output shows semantic status/reason, embedding calls, eligible
candidates, similarity, and the selected goal/YAML separately from cold-exploration model calls.

The provider is a tiny in-process lexical hash embedder, not a learned model or remote vector DB. Its
explicit normalizations are limited to examples such as `check out`/`checkout` and `finish`/`complete`.
This proves local relevance/fallback plumbing only: **general semantic correctness, broad paraphrase
coverage, non-English understanding, and complete adversarial detection are not claimed**. See
[`packages/semantic-local/README.md`](../packages/semantic-local/README.md) and the deterministic WALK-016
tests before relying on a goal outside those examples.

### Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Successful `run`/`explore`/`remember`, `list`, `inspect` of a passing report, or help |
| `1` | Product/unknown/runner failure (including a Maestro executable-launch failure), rejected candidate, or rejected/failed bounded repair |
| `2` | Usage, configuration, report-inspection, or direct I/O error; also no command |
| `3` | Cold exploration was rejected or failed to produce a verified reusable walk |
| `4` | Invalid/corrupt local memory; quarantine it and recover through verified cold exploration |

### Artifacts and clean-state recovery

All runtime state is generated beneath the configured workspace's ignored `.deja/`:

```text
.deja/
  flows/<goal>/<platform>-<flow-sha256>.yaml  # content-addressed verified flows
  memory/walks.json                           # schema-2 exact identities and validation history
  attempts/                                   # cold/remember candidates and bounded raw logs
  patches/                                    # repair diagnosis, diff, prior bytes, verification
  reports/<command>-<timestamp>-<pid>.json    # immutable per-run result
  reports/latest.json                         # atomically replaced latest result
  reports/<run-id>/                           # runner stdout/stderr/combined output
```

`.deja/` is local state, not release evidence. Preserve it for diagnosis or move the whole
directory aside as shown in the quick start; do not edit a checksum-invalid flow into a hit. If
metadata is malformed or unsafe, the CLI exits `4` without launching Maestro. Quarantine the entire
ignored directory, then run verified cold exploration. Never delete curated files under
`spikes/*/evidence/` as part of cleanup. Only reviewed, secret-free reports, logs, flows, and diffs
belong in committed evidence.

## Safety rules

- **Secrets:** use only deterministic fixture values. Raw prompts, model responses, screen content,
  YAML-bearing subprocess output, screenshots, and state artifacts remain ignored. CLI text/JSON
  redacts configured values, credential-like fields, and workspace/home absolute paths, but redaction
  is not permission to commit raw runtime state.
- **Bounded writes:** source repair accepts only canonical, non-symlinked files under configured
  roots, with permitted extensions and exact expected content. It has one attempt and one shared
  deadline. Failed patch/build/install/verification restores original source bytes.
- **No auto-commit:** repair prints and preserves its exact proposed diff. Successful source changes
  remain visible for human review; Déjà never runs Git, commits, or opens a pull request.
- **Product regression rule:** a meaningful product assertion failure is a potential product
  regression, not stale-selector evidence. Do not weaken or rewrite the assertion, patch source, or
  save a candidate to make it pass. Product, unknown, timeout, navigation, runner, and ambiguous
  failures do not enter this repair policy. A narrowly classified selector failure must independently
  pass the unchanged full journey before atomic cache replacement.

See [Architecture](ARCHITECTURE.md) for exact cache identity, memory schemas, plugin contracts,
failure classification, and atomic-write ordering.

## Demo

[`demo/deja-demo.mp4`](../demo/deja-demo.mp4) is a **58-second, 1080p captioned fallback**
built from committed real-run evidence. It covers Android cold exploration, verified YAML, exact
zero-model warm replay, bounded repair, the product-regression guard, and the explicit Chromium
support boundary. It does not claim instant replay. [`demo/SCRIPT.md`](../demo/SCRIPT.md) links every
claim to raw artifacts; [`demo/rehearsal/`](../demo/rehearsal/REVIEW.md) records the rejected 65-second
take and two complete 58-second decode/evidence rehearsals. Regenerate with
`python3 demo/generate_video.py && python3 demo/rehearse.py` (Pillow and FFmpeg required).

## Development

```sh
bun install
bun run lint                  # Oxlint
bun test --timeout=10000      # deterministic unit/component/integration suite
bun run build                 # TypeScript project build + optional viewer production bundle
bun run dev -- --host 127.0.0.1 --port 4173
bun scripts/validate-release-artifacts.ts  # run in a committed, clean tree
```

[`Portable checks`](CI.md) additionally documents the GitHub Actions Linux job, its exact
clean-checkout commands and artifact scans, and why real Android/Chromium flows remain separately
evident local integration gates. CI makes no device, model, or iOS claim.

The Vite app is an **optional read-only local viewer**, not the Déjà runtime. It cannot start
runs or repair and is unnecessary for headless use. See [viewer/README.md](../viewer/README.md).
The package scripts also include `preview` for the built viewer.

## Repository guide and evidence workflow

- [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) — implementation and persistence contracts.
- [`docs/ROUTE_GRAPH.md`](ROUTE_GRAPH.md) — diagnostic-only bounded Android route graphs, exact
  WALK-020 evidence provenance, conservative screen matching, and acceptance limits.
- [`docs/SUBFLOW_COMPOSITION.md`](SUBFLOW_COMPOSITION.md) — WALK-018 typed verified login reuse,
  clean-state composed/exact replay, byte-preservation evidence, and narrow composition limits.
- [`apps/cli/README.md`](../apps/cli/README.md) — CLI adapter details and Web boundary.
- [`spikes/001-android-deja/`](../spikes/001-android-deja/README.md) — preserved Android fixture/evidence.
- [`spikes/002-web-deja/`](../spikes/002-web-deja/README.md) — bounded Chromium fixture/evidence.
- [`spikes/003-ios-deja/`](../spikes/003-ios-deja/README.md) — bounded native iOS simulator fixture/evidence.
- [`TODO.md`](../TODO.md) — standing rules. The backlog, status and dependencies are in the Linear team DEJA.
- [`docs/PROJECT_PLAN.md`](PROJECT_PLAN.md) — historical hackathon scope and plan; current verified behavior is defined by ticket evidence, not aspirations.

The backlog lives in the Linear team DEJA (open work moved there from the old repo-local `WALK-`
tickets, which stay in git history). An issue is done only when all acceptance criteria link
reproducible commands and secret-free evidence in this repo. Record exact commands and measured
values, use `unknown` when instrumentation is unavailable, and preserve prior evidence. Closing an
issue does not authorize social posts, external submission, or other publication.

Licensed under [Apache-2.0](../LICENSE).

