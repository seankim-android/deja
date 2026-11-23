# 004 iOS observation (DEJA-18)

Measures whether stepping through Maestro one action at a time is practical on iOS, and what the
iOS hierarchy gives a fingerprint. Evidence: [evidence/RESULTS.md](evidence/RESULTS.md) (generated),
with the raw per-step data in `evidence/walk-*.json` and `evidence/facts.json`.

Setup used: iPhone 17 simulator (dedicated, iOS 27.2), Xcode 27.2, Maestro 2.12.0 at
`~/.local/share/deja/maestro/2.12.0`, Blankaday Dev build (`com.blankaday.dev`) launched with
`uiTesting`. The machine was heavily loaded (load average 100 to 500), so every sample records the
1-minute load average. The two transports ran one after the other, not at the same load: compare
them with that in mind (a process start measured 21 to 28 s even at load 95 in `facts.json`).

## Reproduce

```sh
export DEVELOPER_DIR="$(<repo>/scripts/ci-xcode.sh)"        # the pinned Xcode, from the Blankaday repo
export DEJA_MAESTRO=~/.local/share/deja/maestro/2.12.0/maestro/bin/maestro
export DEJA_SIM_UDID=<your dedicated simulator UDID>
xcrun simctl create "iPhone 17 (deja)" com.apple.CoreSimulator.SimDeviceType.iPhone-17 \
  com.apple.CoreSimulator.SimRuntime.iOS-27-2     # then boot it and install a Blankaday Dev .app
bun spikes/004-ios-observation/walk.ts process 6  # fresh maestro process per step
bun spikes/004-ios-observation/walk.ts mcp 6      # one long-running `maestro mcp`
bun spikes/004-ios-observation/facts.ts           # selector, keyboard, animation, crash probes
bun spikes/004-ios-observation/summarize.ts       # writes evidence/RESULTS.md
```

Use a simulator only you own. Never run `simctl shutdown all`.

## What went wrong

- First process-transport run: `launchApp` with `-screenshotState history` hit the 180 s timeout
  once at load about 400. The app was left half-started and the next eight steps failed. A rerun
  with a 300 s timeout passed. The failed run's file was replaced by the rerun; this note is its
  only record.
- The Today first-visit hierarchy holds the Journal page too (see RESULTS.md, "Fingerprint
  stability"). With default rules Today, Feed and Settings are gaps (`marker-conflict`).
- Crash and system-alert probes are inconclusive (RESULTS.md items 4 and 5).
- The MCP session's first `run` after a reset can take 30 to 45 s; its p90 for `reset` is poor.
