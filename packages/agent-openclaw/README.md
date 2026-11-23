# OpenClaw cold explorer

`runColdExploration` implements the WALK-003 attempt → independent verification → successful-store lifecycle for one explicitly configured Android fixture. It does not perform warm replay, source repair, semantic retrieval, or cross-platform exploration.

The parent writes `openclaw-config.json` into a fresh ignored attempt directory and invokes the non-interactive headless entry point without a shell:

```text
openclaw agent exec
  --message-file <attempt>/prompt.txt
  --cwd <attempt>/workspace
  --config <attempt>/openclaw-config.json
  --model cerebras/moonshotai-kimi-k3.1-pro-trial
  --thinking off
  --code-mode direct
  --timeout 900
  --json
```

The pinned config declares the exact Cerebras HTTPS endpoint/model catalog entry and a single stdio MCP server. `scripts/maestro-mcp-proxy.ts` launches `maestro --no-ansi --device <device> mcp`, forwards JSON-RPC, and records bounded metadata for initialize/list/call/response activity. Local tools are allowlisted; cloud, viewer, and external flow-file inputs are excluded. Inline YAML is hashed rather than copied into the MCP activity log.

The explorer receives only the goal, explicit app/device configuration, and values under `fixtureCredentials`. Its tool policy is `maestro__*`; filesystem, shell, web, memory, sessions, reference flows, app source, and successful memory are not provided. `--cwd` is an empty attempt workspace with workspace context injection disabled. The final response must be only candidate YAML. The parent validates the exact app ID, a `launchApp` `clearState: true`, and a visible assertion, then writes the YAML only to `<attempt>/candidate.yaml`.

Injected parent hooks clear the app before exploration and again before verification, execute the complete candidate through WALK-002, and persist only a passing result. A thrown error or rejected verification leaves failed YAML/logs in the attempt and never calls successful persistence.

## Metric boundaries

- `explorationDurationMs`: wall time of the `openclaw agent exec` child, measured monotonically by the parent.
- `verificationDurationMs`: parent wall time around the independent WALK-002 hook; the nested runner also reports its own wall and monotonic durations.
- `mcpCalls`: `tools/call` requests observed by the Maestro stdio proxy. It excludes initialize, tool listing/discovery, and OpenClaw dispatch/describe machinery. Missing logs produce `null`/`unknown`; an existing empty log produces zero.
- `modelCalls`: OpenClaw `assistantTurns`, meaning completed assistant/provider round trips. It is not a claim about physical HTTP retries, token counts, or cost. Missing instrumentation produces `null`/`unknown`.
- Overall `durationMs`: the full cold lifecycle, including clean-state setup, exploration, verification, and persistence. This is not a warm-replay comparison or speedup claim.

Raw prompt, model response, YAML-bearing subprocess output, screenshots, and state artifacts stay under ignored `.deja/attempts/`. Curate only the candidate with deterministic public fixture data, redacted MCP/activity logs, structured reports, and model-metric summaries. Never put real credentials in `fixtureCredentials` or committed evidence.

Run focused tests from the repository root:

```sh
bun test packages/agent-openclaw/test/openclaw-explorer.test.ts --timeout=10000 --check
```
