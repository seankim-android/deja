# Headless `walkcache` CLI

Run from the repository root with `bun apps/cli/walkcache.ts …`, or use `bun run walkcache -- …`.
The executable imports only the plugin implementations named by configuration; the orchestrator in
`packages/core` has no OpenClaw, Maestro, React, filesystem, demo, or platform dependency.

```sh
# From the validated Android fixture, after `. ./scripts/env.sh` and build/install:
bun ../../apps/cli/walkcache.ts --help
bun ../../apps/cli/walkcache.ts explore checkout --config walkcache.config.json
bun ../../apps/cli/walkcache.ts run checkout --config walkcache.config.json
bun ../../apps/cli/walkcache.ts remember checkout --flow evidence/repaired-checkout.yaml \
  --config walkcache.config.json
bun ../../apps/cli/walkcache.ts list --config walkcache.config.json
bun ../../apps/cli/walkcache.ts inspect .walkcache/reports/latest.json \
  --config walkcache.config.json
```

The combined [`walkcache.config.json`](../../spikes/001-android-walkcache/walkcache.config.json)
explicitly names workspace, app/build/device, fixture-only credentials, provider/model, bounded
timeouts, executable names, MCP proxy, and the WALK-007 two-file repair policy. `maestro` and
`openclaw` resolve through the sourced fixture `PATH`; `WALKCACHE_MAESTRO`,
`WALKCACHE_OPENCLAW`, and `WALKCACHE_TIMEOUT_MS` can override defaults when the corresponding
JSON fields are omitted. Android SDK/Java locations come only from the environment and are not
serialized into reports.

`run` computes the exact build identity and completes local lookup before either lazy plugin
factory: a warm hit never constructs OpenClaw/MCP or invokes a model. A normal miss uses the
verified cold lifecycle. A stale-build miss goes directly to bounded selector repair; core never
substitutes blind exploration after a repair rejection. Product assertion and unknown failures
stop without repair. `remember` independently verifies YAML and never saves a failed candidate.
Generated YAML still runs directly with `maestro test` and has no WalkCache/LLM dependency.

Output always includes outcome, cache status/reason, model-call basis, duration, assertions, repair
count, YAML/report paths, errors/failure category, artifacts, and diff state. Repair output includes
the exact proposed source diff and `autoCommit: false`. Reports are atomic under ignored
`.walkcache/reports/`; raw model, screen, and subprocess logs stay ignored. Terminal and JSON output
redacts configured fixture values, credentials-like fields, workspace/home absolute paths. Generated
YAML may contain deterministic fixture inputs but is never printed by the CLI summary.

Exit codes are `0` success/help/list/inspect-of-pass, `1` product/unknown/repair/candidate failure,
`2` usage/config/report/I/O error, `3` rejected or failed cold exploration, and `4` invalid memory.
Inspecting a failed report returns that report's original failure class; inspecting a passing report
returns zero. Help does not load configuration. Invalid memory must be quarantined, not edited into
a hit.

## WALK-010 Web boundary

[`spikes/002-web-walkcache`](../../spikes/002-web-walkcache/README.md) adds a distinct Maestro Web
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
