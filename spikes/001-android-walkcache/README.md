# Spike 001: Android WalkCache proof of concept

## Verdict: VALIDATED

**Question:** can an OpenClaw/Kimi agent explore this Android app through Maestro, save a
successful walk as YAML, and have a subsequent run replay it with zero model calls?

This spike intentionally uses native Java Android instead of Expo or the future SDK layout. It
tests feasibility before production abstractions. Do not merge this code into a production
package without rewriting it normally.

## Fixture

- App ID: `dev.walkcache.checkout`
- Journey: login -> add Mochi sticker pack -> checkout -> order confirmation
- Test-only input: `qa@walkcache.dev` / `testing123` (accepted by the local fixture app)
- Missing selector: the Checkout button has text but no stable resource ID in the first iteration
- Product assertion: `Order #WC-1001` must appear after checkout

## Checks

- [x] Dedicated Android API 34 emulator boots and is visible to Maestro
- [x] App builds and installs on that emulator
- [x] `flows/checkout.yaml` passes all 15 steps as a manual reference baseline
- [x] A cold agent run produces an independent verified flow without reading the baseline
- [x] A warm run replays saved YAML with zero model calls
- [x] One stale-selector or failure case is exercised

## Evidence

- Android SDK/API 34, emulator `walkcache_api_34` / `emulator-5554`, Android 14.
- Maestro CLI 2.12.0 at `~/.maestro/bin/maestro`; OpenJDK 21.0.13.
- Gradle wrapper 8.11.1 and Android Gradle Plugin 8.9.3; `./gradlew :app:assembleDebug` passed.
- `adb -s emulator-5554 install -r -g app/build/outputs/apk/debug/app-debug.apk` passed.
- `maestro --no-ansi --device emulator-5554 test flows/checkout.yaml` passed all 15 steps,
  including the final `Order #WC-1001.*` assertion and screenshot.

### Cold exploration

- A separate OpenClaw/Kimi agent was denied access to the reference flow and app source. It used
  only the QA goal, fixture inputs, and live Maestro MCP screen inspection/actions.
- `.walkcache/attempts/mcp-calls.jsonl` contains 21 MCP calls.
- Its independently authored `.walkcache/candidate/checkout-android.yaml` replayed all 21
  commands successfully and asserted both `Order confirmed` and the exact fixture order.
- Two YAML authoring failures (unquoted `$5.00` and an unquoted `#`) were corrected after syntax
  failures and fresh hierarchy inspection; no UI exploration branch failed.
- Parent-controlled `bun scripts/walkcache.ts remember checkout --flow
  .walkcache/candidate/checkout-android.yaml` independently passed in 25.101s before storing it.

### Warm replay

- `bun scripts/walkcache.ts run checkout` found an exact goal/app/platform/APK-fingerprint match,
  ran stored YAML directly, emitted `cacheStatus: "hit"` and `modelCalls: 0`, and passed in 24.824s.
- This proves model avoidance, not a runtime speedup: verification and warm Maestro execution both
  took about 25s. The full cold exploration duration and per-turn model-call count are not claimed.

### Stale selector stress

- Changing stored YAML to target `checkout_button` before that ID existed failed only at Checkout,
  after all preceding steps passed, and emitted `passed: false` with `modelCalls: 0`.
- The minimal source repair adds `checkout_button` to `ids.xml` and uses `R.id.checkout_button`
  for the existing Checkout button. It adds no other IDs or product changes.

- Adding the ID changed the APK fingerprint, so the old cached walk correctly returned
  `cacheStatus: \"miss\"`, `modelCalls: 0`, and exit code 2 without invoking exploration.
- Independent repaired-flow verification passed in 25.784s before replacing memory.
- The new warm cache hit passed in 24.510s with `modelCalls: 0`.
- Exact evidence is curated in `evidence/`: the original agent-authored flow, repaired flow, and
  minimal source diff. Larger generated logs remain ignored under `.walkcache/`.

## What this validates

An OpenClaw/Kimi agent can learn an Android QA journey through Maestro MCP; a local learning layer
can verify and persist that walk; later runs can bypass the model entirely; and a minimal selector
repair can be verified before the cache is replaced.

## Known limitations / next production step

- Cold exploration was orchestrated as an isolated OpenClaw/Kimi session, not yet integrated into
  `walkcache.ts` as one self-contained `explore <goal>` CLI command.
- The spike does not measure a total cold-vs-warm speedup or claim precise cold model-call counts.
- Cache lookup is exact/local. Semantic retrieval, route graphs, and subflow composition are future work.
- Selector failure vs product regression classification is not generalized yet. No autonomous repair
  loop is claimed, and a deliberate product-regression negative test remains in the root TODO.
- Evidence is Android emulator only. Web and iOS have not been attempted by this spike.
- The production step is to rewrite this proven path behind small runner/store/explorer interfaces,
  then connect cold exploration to the same CLI lifecycle without weakening verification gates.

## Reproduction commands

```sh
cd spikes/001-android-walkcache
. ./scripts/env.sh
./gradlew :app:assembleDebug
adb -s emulator-5554 install -r -g app/build/outputs/apk/debug/app-debug.apk

# MCP bridge used by the exploring agent:
bun scripts/mcp-call.ts inspect_screen '{"device_id":"emulator-5554"}'

# Independently verify/store an agent candidate, then run a matching cache hit:
bun scripts/walkcache.ts remember checkout --flow .walkcache/candidate/checkout-android.yaml
bun scripts/walkcache.ts run checkout
cat .walkcache/reports/latest.json
```

The final checked-in fixture already includes the repair. To reproduce the original stale-selector
failure, remove `checkout_button` from `ids.xml`, pass `null` for the Checkout button ID in
`MainActivity.java`, rebuild/install, and run `evidence/agent-cold-checkout.yaml` after changing its
Checkout step to target the full `checkout_button` resource ID. Reapply
`evidence/checkout-selector-repair.diff` to return to the repaired state.

