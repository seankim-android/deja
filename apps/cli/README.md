# Headless `deja` CLI

Run from the repository root with `bun apps/cli/deja.ts …`, or use `bun run deja -- …`.
The executable imports only the plugin implementations named by configuration; the orchestrator in
`packages/core` has no OpenClaw, Maestro, React, filesystem, demo, or platform dependency.

```sh
# From the validated Android fixture, after `. ./scripts/env.sh` and build/install:
bun ../../apps/cli/deja.ts --help
bun ../../apps/cli/deja.ts explore checkout --config deja.config.json
bun ../../apps/cli/deja.ts run checkout --config deja.config.json
bun ../../apps/cli/deja.ts remember checkout --flow flows/repaired-checkout.yaml \
  --config deja.config.json
bun ../../apps/cli/deja.ts list --config deja.config.json
bun ../../apps/cli/deja.ts inspect .deja/reports/latest.json \
  --config deja.config.json
```

The combined [`deja.config.json`](../../spikes/001-android-deja/deja.config.json)
explicitly names workspace, app/build/device, fixture-only credentials, provider/model, bounded
timeouts, executable names, MCP proxy, and the WALK-007 two-file repair policy. `maestro` and
`openclaw` resolve through the sourced fixture `PATH`; `DEJA_MAESTRO`,
`DEJA_OPENCLAW`, and `DEJA_TIMEOUT_MS` can override defaults when the corresponding
JSON fields are omitted. Android SDK/Java locations come only from the environment and are not
serialized into reports.

`run` computes the exact build identity and completes local lookup before either lazy plugin
factory: a warm hit never constructs OpenClaw/MCP or invokes a model. A normal miss uses the
verified cold lifecycle. A stale-build miss goes directly to bounded selector repair; core never
substitutes blind exploration after a repair rejection. Product assertion and unknown failures
stop without repair. `remember` independently verifies YAML and never saves a failed candidate.
Generated YAML still runs directly with `maestro test` and has no Déjà/LLM dependency.

Output always includes outcome, cache status/reason, model-call basis, duration, assertions, repair
count, YAML/report paths, errors/failure category, artifacts, and diff state. Repair output includes
the exact proposed source diff and `autoCommit: false`. Reports are atomic under ignored
`.deja/reports/`; raw model, screen, and subprocess logs stay ignored. Terminal and JSON output
redacts configured fixture values, credentials-like fields, workspace/home absolute paths. Generated
YAML may contain deterministic fixture inputs but is never printed by the CLI summary.

Exit codes are `0` success/help/list/inspect-of-pass, `1` product/unknown/runner/executable-launch/
repair/candidate failure, `2` usage/config/report/direct-I/O error, `3` rejected or failed cold
exploration, `4` invalid memory, and `5` the store is locked by another run.
Inspecting a failed report returns that report's original failure class; inspecting a passing report
returns zero. Help does not load configuration. Invalid memory must be quarantined, not edited into
a hit.

## One store per device

A store has one writer. To split flows over devices, start one `deja` per device with its own
store: `deja run checkout --store "$UDID"`. Each id keeps its own history under
`.deja/memory/stores/<id>/`; flows are shared. Two runs on the same store are refused with exit 5
before they run anything, and `deja list` works while a run holds the lock. See
[ARCHITECTURE.md](../../docs/ARCHITECTURE.md#one-writer-per-store-one-store-per-device-deja-4).

## JUnit and summary output

`--junit <file>` (or config `junit`) has Maestro write its own JUnit report there. When Maestro never
ran or did not pass, a minimal failed report is written instead, so a host never reads a missing file
as a pass. `--summary` prints one `deja-summary v=1 …` line (`goal=` last, percent-encoded). Hosts must treat a missing file as a failure. Both are opt-in; exit codes do not
change. Details: [docs/REFERENCE.md](../../docs/REFERENCE.md#junit-and-summary-output-for-ci-hosts).

## WALK-016 opt-in semantic lookup

Add `semantic: { "provider": "local-lexical-v1" }` to enable the local deterministic proof. Optional
`minimumSimilarity` (default `0.90`) and `minimumMargin` (default `0.15`) accept values from those
defaults through one only; they may be raised but never lowered. Zero, null, non-numbers, NaN, and infinity are
rejected. Omitting `semantic` preserves prior behavior; a present config with a missing/unsupported
provider reports `provider-unavailable` and continues the ordinary miss/explore lifecycle rather than
manufacturing a hit.

Exact lookup always runs first with zero provider constructions and embedding calls. Only exact
`not-found` invokes semantic retrieval. Candidates must pass the same full store identity, canonical
path/checksum, and latest-validation gates before candidate embedding/ranking. Ambiguous,
low-confidence, unrelated, contradictory, and narrow adversarial-marker goals select no walk;
adversarial markers stop before cold exploration. Other misses preserve the normal cold lifecycle.
The local provider rejects raw hash collisions with a canonical-token guard. After provider calls,
the winner is exactly re-looked-up immediately before replay; a changed winner executes nothing.

A semantic hit replays the unchanged selected YAML and records validation against that stored goal,
not the paraphrase. It never bypasses verification or enters semantic-driven repair. Output includes
semantic provider/status/reason, embedding calls, candidate counts, similarity/runner-up, and selected
goal/flow. Model-call metrics remain separate. The fixed local lexical vocabulary and limitations are
documented in [`packages/semantic-local/README.md`](../../packages/semantic-local/README.md); this is
not generalized semantic correctness or a remote vector service.

## WALK-010 Web boundary

[`spikes/002-web-deja`](../../spikes/002-web-deja/README.md) adds a distinct Maestro Web
execution adapter for the installed 2.12.0 Beta Chromium runtime. Its configuration explicitly
requires `platform: web`, `browser: chromium`, an adapter identity, loopback `appId`, `device:
chromium`, browser settings, and a static build tree. The adapter owns browser command selection,
static-tree fingerprinting, and its adapter/browser contribution to exact cache identity; core and
Android repair contain no per-demo Web conditionals. Existing Android identity remains unchanged.

Only independently verified YAML (`remember`), exact Chromium warm replay, build invalidation, and
honest assertion failures are supported. Web cold model exploration returns
`exploration-rejected` with 0 model calls and directs the operator to reviewed YAML + `remember`.
Web has no repair plugin: a stale build returns `repair-rejected` and never falls through to cold
exploration. Android MCP `launchApp/clearState`, APK hashing/install, and Android source patching
are deliberately not reused. No cross-browser, non-loopback, generalized exploration, or measured
speedup claim is made. Actual commands/results are linked from the Web fixture README.

## WALK-011 iOS boundary

[`spikes/003-ios-deja`](../../spikes/003-ios-deja/README.md) adds a distinct native iOS
execution adapter. Configuration requires `platform: ios`, a fixture bundle identifier, an `.app`
build tree, an explicit simulator UDID, and adapter identity. The adapter hashes every regular file
and relative path in the native bundle, includes the adapter in exact cache identity, and executes
Maestro with `--platform ios --device <UDID>`. It never shares Android install/state/repair logic.

Only independently reviewed YAML via `remember`, exact zero-model warm replay, native-build
invalidation, and meaningful product-regression handling are supported. Cold iOS exploration returns
`exploration-rejected` with 0 model calls; stale builds return `repair-rejected` without a cold
fallback, unless the config opts in with `"staleBuild": "replay-last-verified"` (WALK-021): then the
last verified YAML replays once on the new build, a pass is stored for it, and a fail stores nothing.
A pass replaces the old build's entry and restarts its history (DEJA-12 tracks keeping it).

The verified target is the dedicated iPhone 17 / iOS 27.0 `walkcache_ios` simulator with
Xcode 27.2 and Maestro 2.12.0. The requested Android compatibility replay was blocked before boot
by emulator disk capacity, as recorded in the WALK-011 evidence; portable tests do not replace it.

## Manifest (DEJA-36)

Set `"manifest": "deja.walks.json"` in the config to replay your own flow files in place. The
manifest is committed with them. `run --verify` re-checks a changed flow, `remember
--accept-assertion-change` accepts changed assertions, and exit code 6 means the flow changed, was
never validated, needs migration, or its assertions changed. See
[docs/REFERENCE.md](../../docs/REFERENCE.md#consumer-owned-flows-and-the-walk-manifest).
