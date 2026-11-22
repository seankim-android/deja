---
id: WALK-003
title: Add a self-contained cold explore goal lifecycle
milestone: M2B–M2C
priority: P0
status: done
depends_on: [WALK-002]
---

# WALK-003 — Add a self-contained cold `explore <goal>` lifecycle

## Outcome

Replace the manually coordinated isolated agent session with one reproducible command that explores
from a natural-language goal, independently verifies the candidate, and persists it only on success.

## Checklist

- [x] Add `walkcache explore <goal>` with explicit app, platform, build, and workspace configuration.
- [x] Use a documented non-interactive OpenClaw/Kimi invocation and Maestro MCP transport.
- [x] Deny the explorer the reference flow and app source; provide only the goal and allowed fixture
  credentials/configuration.
- [x] Start from clean app state, record MCP activity, and keep failed branches in attempt artifacts.
- [x] Write candidate YAML to attempts, never directly to successful flow storage.
- [x] Verify the complete candidate through WALK-002 from a fresh `clearState` launch.
- [x] Persist through the atomic store only after independent verification passes.
- [x] Report exploration duration, verification duration, MCP calls, and model calls when available;
  emit `null`/`unknown` rather than guessing unavailable values.
- [x] Prove a rejected candidate does not replace the last known-good flow.

## Acceptance evidence

From a clean candidate/cache state, run one command for the Android checkout goal. Link the authored
YAML, MCP activity log, independently passing report, and stored metadata. Repeat with a deliberately
invalid candidate and show that no successful memory was created or replaced.

## Reproducible evidence

### Positive Android cold lifecycle

The acceptance run began with `.walkcache/flows`, `.walkcache/memory`, and `.walkcache/candidate` absent.
The validated WALK-001/002 versions were moved intact to ignored `.tmp/walk003-validated-baseline/`, not
deleted or weakened. The emulator remained `emulator-5554` / `walkcache_api_34` throughout.

From `spikes/001-android-walkcache`, after the documented build/install and `. ./scripts/env.sh` setup,
the single lifecycle command was:

```sh
WALKCACHE_OPENCLAW="$(command -v openclaw)" \
  bun scripts/walkcache.ts explore checkout --config explore.config.json
```

[`explore.config.json`](../spikes/001-android-walkcache/explore.config.json) explicitly configures the
workspace, `dev.walkcache.checkout`, Android, APK path, device, Cerebras HTTPS endpoint, Kimi model,
timeouts, and public deterministic fixture credentials. The exact non-interactive invocation and MCP
stdio proxy are documented in [`packages/agent-openclaw/README.md`](../packages/agent-openclaw/README.md).
The pinned run allows only `maestro__*`; the attempt workspace has no app source/reference flow, file,
shell, web, memory, session, successful-store, external-flow, or script access. OpenClaw uses the
operator's existing Cerebras authentication without serializing its credential.

Acceptance artifacts:

- [Authored candidate YAML](../spikes/001-android-walkcache/evidence/explore-candidate.yaml): exact
  OpenClaw final response, saved only in the attempt before verification. Its 21 steps cover login,
  catalog, cart, checkout, and assertions for `Order confirmed` and `Order #WC-1001 · Mochi sticker pack`.
- [MCP activity](../spikes/001-android-walkcache/evidence/explore-mcp-activity.jsonl) and
  [lifecycle activity](../spikes/001-android-walkcache/evidence/explore-activity.jsonl): 13
  `tools/call` requests with bounded argument metadata, response durations/errors, clean-state events,
  verification, and persistence. Request id 7 records a failed exploration branch (`isError: true`);
  inline YAML is represented only by byte count/SHA-256.
- [Independently passing report](../spikes/001-android-walkcache/evidence/explore-android-report.json):
  `passed: true`, `cacheStatus: "recorded"`, runner `WALK-002 runMaestro`, fresh clear-state launch,
  runner exit code 0, runner duration 26,404 ms, verification wrapper 26,406 ms, exploration 80,598 ms,
  overall lifecycle 107,297 ms, 13 MCP calls, and 15 model calls. APK SHA-256 is
  `4746296fa8aa84ae85761915f651b1488c38ef8f5f4e89af0e9ad19784cfcb05`.
- [Stored metadata](../spikes/001-android-walkcache/evidence/explore-stored-memory.json): atomic store
  metadata and verified flow SHA-256
  `064e0621edf6c3e538fc717d71fca1395dd4cf32f09ec1e8a87f002959748f08`.
- [Model metric envelope summary](../spikes/001-android-walkcache/evidence/explore-model-metrics.json):
  Cerebras `moonshotai-kimi-k3.1-pro-trial`, 15 `assistantTurns`, 13 MCP bridge calls, and unavailable
  usage/cost instrumentation. Raw prompts, responses, output, and generated artifacts remain ignored.

### Deliberately rejected candidate and preservation

The negative proof uses the same one-command lifecycle with a deterministic fake explorer, not a claim
of another real Kimi run. It authors a fresh-clearState candidate that deliberately asserts the absent
`Order #WC-9999`:

```sh
WALKCACHE_OPENCLAW="$PWD/../../packages/agent-openclaw/test/fixtures/fake-openclaw.cjs" \
WALKCACHE_FAKE_OPENCLAW_MODE=invalid WALKCACHE_FAKE_OPENCLAW_NO_TURNS=1 \
  bun scripts/walkcache.ts explore checkout --config explore.config.json
```

[The rejected candidate](../spikes/001-android-walkcache/evidence/explore-rejected-candidate.yaml),
[rejected report](../spikes/001-android-walkcache/evidence/explore-rejected-report.json), and
[activity](../spikes/001-android-walkcache/evidence/explore-rejected-activity.jsonl) record a real
WALK-002 failure at that assertion: Maestro exit code 1, runner duration 22,630 ms, verification wrapper
22,631 ms, `cacheStatus: "rejected"`, and `persistence: null`. The fixture invoked no MCP/model services,
so metrics are 0 MCP calls and `modelCalls: null`, `modelCallBasis: "unknown"`.

[Byte-preservation evidence](../spikes/001-android-walkcache/evidence/explore-rejection-preservation.json)
confirms both files are unchanged before/after rejection:

- Successful YAML SHA-256: `064e0621edf6c3e538fc717d71fca1395dd4cf32f09ec1e8a87f002959748f08`.
- Successful memory SHA-256: `1b13b9d3bce7c291e23d520650c549734cb9361577485023b494227d218eaa9d`.

No new successful walk was created and no last-known-good flow or memory was replaced.

### Quality gates and measurement boundaries

From the repository root, reproduce focused unit/runner coverage and static gates:

```sh
bun test packages/agent-openclaw/test/openclaw-explorer.test.ts \
  packages/maestro/test/maestro-runner.test.ts --timeout=10000 --check
bun run lint
bun run build   # tsc -b plus Vite production build
git diff --check
```

Recorded output: [unit tests](../spikes/001-android-walkcache/evidence/explore-unit-tests.txt)
(10 tests, 97 assertions, 0 failures, including all six unchanged WALK-002 runner tests),
[lint](../spikes/001-android-walkcache/evidence/explore-lint.txt) (0 warnings/errors), and
[type-check/build](../spikes/001-android-walkcache/evidence/explore-build.txt) (passed).

`modelCalls` is OpenClaw `assistantTurns`, i.e. completed assistant/provider round trips, not a physical
HTTP retry/token/cost count. Missing values stay `null`/`unknown`. `mcpCalls` counts only proxied
Maestro `tools/call` requests, excluding initialization, listing, discovery, and OpenClaw dispatch
wrappers. Exploration is the OpenClaw child wall time; verification wrapper time includes the WALK-002
hook, whose nested runner timings are also retained. These measurements do not claim a warm-run speedup.

### Non-goals

Warm replay optimization, semantic retrieval, source repair, and support for additional platforms.

