# WalkCache

[![Portable checks](https://github.com/seankim-android/kimi-hackathon/actions/workflows/portable-checks.yml/badge.svg)](https://github.com/seankim-android/kimi-hackathon/actions/workflows/portable-checks.yml)

**Explore once. Replay without a model. Repair only bounded staleness.**

WalkCache is a local, headless learning layer around [Maestro](https://maestro.dev/). On Android,
an OpenClaw agent using Kimi explores a QA journey, independently verifies the resulting Maestro
YAML, and stores it only after the complete journey passes. Later runs replay that YAML directly:
cache hits construct neither the explorer nor a model client and make **zero model calls**.

The current repair policy is deliberately narrow. It handles only the configured Android Checkout
selector change, writes only inside explicitly configured source roots, verifies the unchanged
journey before replacing cache memory, shows the diff, and never invokes Git or auto-commits.

## Verified support and limitations

| Platform | Verified behavior | Not supported or not claimed |
| --- | --- | --- |
| Android emulator, API 34, Maestro 2.12.0 | Cold exploration, independent verification/`remember`, exact warm replay, build invalidation, one bounded Checkout selector repair, and genuine product-regression handling | General-purpose repair, other apps/selectors without a new policy, physical devices, or other Android versions |
| Chromium Desktop via Maestro Web 2.12.0 **Beta/Experimental** | Independently reviewed YAML via `remember`, exact warm replay, static-build invalidation, and product-regression handling on the loopback fixture | **Cold exploration and repair are unsupported.** Firefox, Safari, mobile web, non-loopback targets, and parallel sharding are not claimed |
| iPhone 17 simulator, iOS 27.0, Xcode 27.2, Maestro 2.12.0 | Independently reviewed YAML via `remember`, exact warm replay, native `.app` build invalidation, and product-regression handling | **Cold exploration and repair are unsupported.** Physical devices, other runtimes/models, and other Maestro versions are not claimed |

Shared Maestro YAML does not establish cross-platform support. See the
[Android evidence](spikes/001-android-walkcache/README.md),
[Chromium fixture and command boundary](spikes/002-web-walkcache/README.md),
[native iOS fixture and command boundary](spikes/003-ios-walkcache/README.md), and
[WALK-010](tickets/WALK-010-verified-web-proof.md)/[WALK-011](tickets/WALK-011-verified-ios-proof.md)
for the evidence behind this matrix. The post-WALK-011 Android compatibility attempt was blocked
before boot by insufficient emulator disk space; its exact limit is linked from WALK-011.

**No measured wall-clock speedup is claimed for the current CLI.** Warm replay eliminates model
calls; it does not make Maestro itself instant. Historical WALK-005 spike samples measured complete
cold commands at 96.5–97.0 seconds and warm commands at 24.3–25.9 seconds, while nested Maestro
runner ranges overlapped. Those historical spike measurements are not a current end-to-end CLI
benchmark or a performance guarantee. Current reports record per-run durations and metric scope;
compare like-for-like lifecycles before drawing timing conclusions. See
[the historical comparison](spikes/001-android-walkcache/evidence/walk005-comparison.md).

## Five-minute Android quick start

This path needs **no model credentials**. It independently verifies the reviewed fixture flow with
`remember`, then performs the first verified warm replay. The five-minute target assumes the
prerequisites are installed and the emulator is already running; SDK installation, AVD creation,
and emulator boot are excluded.

### Prerequisites

- macOS or Linux with [Bun](https://bun.sh/) 1.4 or newer;
- OpenJDK 21, Android SDK command-line tools, `adb`, and a running Android 14/API 34 emulator;
- Maestro 2.12.0 available on `PATH` (only this version is covered by current evidence);
- an emulator serial matching the fixture config, `emulator-5554` (or update the local config);
- OpenClaw plus its configured Cerebras/Kimi credential **only for optional cold exploration**.

The fixture's [`scripts/env.sh`](spikes/001-android-walkcache/scripts/env.sh) contains example
Homebrew paths. Source it on a matching machine, or set equivalent `ANDROID_HOME`,
`ANDROID_SDK_ROOT`, `JAVA_HOME`, and `PATH` values for your installation without committing local
paths.

### Run from a clean checkout

```sh
git clone https://github.com/seankim-android/kimi-hackathon.git
cd kimi-hackathon
bun install
cd spikes/001-android-walkcache
. ./scripts/env.sh

# Confirm the already-running emulator named in walkcache.config.json.
adb devices

# Build/install the deterministic fixture and restore its stable selector mode.
./gradlew clean :app:assembleDebug
adb -s emulator-5554 install -r -g app/build/outputs/apk/debug/app-debug.apk
sh scripts/fixture-mode.sh stable

# A fresh clone has no cache. On a reused workspace, preserve old generated state instead.
if [ -d .walkcache ]; then
  mkdir -p .tmp
  mv .walkcache ".tmp/pre-quickstart-$(date +%s)"
fi

# Independently execute and verify reviewed YAML before it can become memory.
bun ../../apps/cli/walkcache.ts remember checkout \
  --flow evidence/repaired-checkout.yaml --config walkcache.config.json

# Exact cache hit: direct Maestro replay, no OpenClaw/MCP construction, 0 model calls.
bun ../../apps/cli/walkcache.ts run checkout --config walkcache.config.json

# Read the redacted structured result.
bun ../../apps/cli/walkcache.ts inspect .walkcache/reports/latest.json \
  --config walkcache.config.json
```

The final `run` must report `Outcome: passed`, `Cache status: hit`, `Model calls: 0`, and a nonzero
assertion count. `remember` is verification of supplied YAML, not model-led exploration. Generated
YAML remains directly runnable with `maestro test`, without WalkCache or an LLM.

For Android cold exploration after the quick start, quarantine any cache you want to preserve and run:

```sh
bun ../../apps/cli/walkcache.ts explore checkout --config walkcache.config.json
```

Cold exploration uses the existing OpenClaw provider authentication. It does not write provider
secrets into configuration, prompts, reports, or MCP activity logs.

## CLI

From a configured fixture directory, use `bun ../../apps/cli/walkcache.ts …`; from the repository
root, use `bun run walkcache -- … --config <path>`. Help works without configuration:

```sh
bun run walkcache -- --help
```

| Command | Behavior |
| --- | --- |
| `explore <goal>` | Force Android cold exploration; independently verify and persist only a passing candidate. Web and iOS return `exploration-rejected` with zero model calls. |
| `run <goal>` | Compute exact identity and check memory first. A hit replays YAML; a normal miss explores; eligible Android stale-build memory enters one bounded repair. A failed hit is recorded and is not reusable. |
| `remember <goal> --flow <candidate.yaml>` | Independently execute reviewed YAML and persist only after a complete pass. A bad product assertion remains a failure. |
| `list` | List valid local walks; does not invoke a runner, explorer, or model. |
| `inspect <report>` | Print a saved, redacted report. Inspecting a failed result returns its original failure class. |

Global options are `--config <file>` (default `./walkcache.config.json`), `--json`, and
`-h`/`--help`. `--flow` is valid only with `remember`. Goals may contain multiple words; quote them
as one shell argument.

### Configuration

The checked-in [Android config](spikes/001-android-walkcache/walkcache.config.json) and
[Chromium config](spikes/002-web-walkcache/walkcache.config.json) are complete fixture examples.
Paths are resolved from the config/workspace as described below; use separate configs for separate
workspaces.

| Field | Meaning |
| --- | --- |
| `workspaceRoot` | Canonical workspace boundary, relative to the config file. `.walkcache/` lives here. |
| `appId`, `platform`, `buildPath`, `device` | App/platform identity, APK or static tree to fingerprint, and execution target. |
| `openClawModel`, `openClawBaseUrl`, `explorationTimeoutSeconds` | Android cold-exploration model/provider and bound. |
| `verificationTimeoutMs`, `maestro.timeoutMs` | Independent verification and Maestro execution limits in milliseconds. |
| `fixtureCredentials` | Explicit deterministic fixture values only; never real user credentials. Values are redacted from CLI output/reports. |
| `maestro.executable`, `openClaw.executable`, `openClaw.mcpProxy` | Executable names and the workspace-local Android MCP proxy. |
| `repair.sourceRoots`, `repair.permittedFileTypes`, `repair.affectedFiles`, `repair.timeBudgetMs` | Closed Android repair allowlist and one shared deadline. No repair block is permitted for Web. |
| Web-only `adapter`, `browser`, `headless`, `screenSize` | Exact execution/cache identity and Chromium launch settings. Only loopback `appId` URLs and `browser: chromium` are accepted. |
| iOS `adapter`, `.app` `buildPath`, simulator-UDID `device` | Exact native bundle-tree/adapter identity and explicit `--platform ios` execution on the dedicated simulator. No repair block is permitted. |

When a corresponding JSON executable/timeout field is omitted, `WALKCACHE_MAESTRO`,
`WALKCACHE_OPENCLAW`, and `WALKCACHE_TIMEOUT_MS` supply defaults. `ANDROID_HOME`/
`ANDROID_SDK_ROOT` and `JAVA_HOME` come from the environment and are not serialized into reports.
The spike also recognizes its more specific repair override; the production config's documented
budget is `repair.timeBudgetMs`.

### Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Successful `run`/`explore`/`remember`, `list`, `inspect` of a passing report, or help |
| `1` | Product/unknown/runner failure (including a Maestro executable-launch failure), rejected candidate, or rejected/failed bounded repair |
| `2` | Usage, configuration, report-inspection, or direct I/O error; also no command |
| `3` | Cold exploration was rejected or failed to produce a verified reusable walk |
| `4` | Invalid/corrupt local memory; quarantine it and recover through verified cold exploration |

### Artifacts and clean-state recovery

All runtime state is generated beneath the configured workspace's ignored `.walkcache/`:

```text
.walkcache/
  flows/<goal>/<platform>-<flow-sha256>.yaml  # content-addressed verified flows
  memory/walks.json                           # schema-2 exact identities and validation history
  attempts/                                   # cold/remember candidates and bounded raw logs
  patches/                                    # repair diagnosis, diff, prior bytes, verification
  reports/<command>-<timestamp>-<pid>.json    # immutable per-run result
  reports/latest.json                         # atomically replaced latest result
  reports/<run-id>/                           # runner stdout/stderr/combined output
```

`.walkcache/` is local state, not release evidence. Preserve it for diagnosis or move the whole
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
  remain visible for human review; WalkCache never runs Git, commits, or opens a pull request.
- **Product regression rule:** a meaningful product assertion failure is a potential product
  regression, not stale-selector evidence. Do not weaken or rewrite the assertion, patch source, or
  save a candidate to make it pass. Product, unknown, timeout, navigation, runner, and ambiguous
  failures do not enter this repair policy. A narrowly classified selector failure must independently
  pass the unchanged full journey before atomic cache replacement.

See [Architecture](docs/ARCHITECTURE.md) for exact cache identity, memory schemas, plugin contracts,
failure classification, and atomic-write ordering.

## Demo

[`demo/walkcache-demo.mp4`](demo/walkcache-demo.mp4) is a **58-second, 1080p captioned fallback**
built from committed real-run evidence. It covers Android cold exploration, verified YAML, exact
zero-model warm replay, bounded repair, the product-regression guard, and the explicit Chromium
support boundary. It does not claim instant replay. [`demo/SCRIPT.md`](demo/SCRIPT.md) links every
claim to raw artifacts; [`demo/rehearsal/`](demo/rehearsal/REVIEW.md) records the rejected 65-second
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

[`Portable checks`](docs/CI.md) additionally documents the GitHub Actions Linux job, its exact
clean-checkout commands and artifact scans, and why real Android/Chromium flows remain separately
evident local integration gates. CI makes no device, model, or iOS claim.

The Vite app is an **optional read-only local viewer**, not the WalkCache runtime. It cannot start
runs or repair and is unnecessary for headless use. See [viewer/README.md](viewer/README.md).
The package scripts also include `preview` for the built viewer.

## Repository guide and evidence workflow

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — implementation and persistence contracts.
- [`apps/cli/README.md`](apps/cli/README.md) — CLI adapter details and Web boundary.
- [`spikes/001-android-walkcache/`](spikes/001-android-walkcache/README.md) — preserved Android fixture/evidence.
- [`spikes/002-web-walkcache/`](spikes/002-web-walkcache/README.md) — bounded Chromium fixture/evidence.
- [`spikes/003-ios-walkcache/`](spikes/003-ios-walkcache/README.md) — bounded native iOS simulator fixture/evidence.
- [`TODO.md`](TODO.md) and [`tickets/README.md`](tickets/README.md) — repo-local backlog, status, and dependencies.
- [`docs/PROJECT_PLAN.md`](docs/PROJECT_PLAN.md) — historical hackathon scope and plan; current verified behavior is defined by ticket evidence, not aspirations.

WalkCache uses repo-local `WALK-` tickets, not Linear objects. A ticket becomes `done` only when
all acceptance criteria link reproducible commands and secret-free evidence; its ticket checklist,
evidence links, and `TODO.md` state change together. Record exact commands and measured values, use
`unknown` when instrumentation is unavailable, and preserve prior evidence. Completing a repository
ticket does not authorize social posts, external submission, or other publication.

Licensed under [Apache-2.0](LICENSE).
