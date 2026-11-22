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

### Self-contained cold lifecycle (WALK-003)

After building/installing the fixture and sourcing `scripts/env.sh`, one command runs the complete cold lifecycle:

```sh
WALKCACHE_OPENCLAW="$(command -v openclaw)" \
  bun scripts/walkcache.ts explore checkout --config explore.config.json
```

`explore.config.json` explicitly pins the workspace, Android app/package, APK build, emulator, Cerebras
HTTPS endpoint, Kimi model, timeouts, and deterministic fixture-only credentials. The generated pinned
OpenClaw config invokes `openclaw agent exec --json` with only `maestro__*` tools. A stdio proxy starts
Maestro MCP and records redacted JSON-RPC activity without copying inline YAML or screen contents into
the activity log. See [`packages/agent-openclaw/README.md`](../../packages/agent-openclaw/README.md)
for the invocation, denial, and metric contracts.

The parent clears only `dev.walkcache.checkout` before exploration and again before independent
verification. The explorer has an empty attempt workspace, no file/shell/web/memory/session tools, no
reference flow or app source, and no successful-memory path. Its final YAML is written to a fresh
`.walkcache/attempts/explore-*/candidate.yaml`. Only a complete passing WALK-002 `runMaestro` verification
calls the atomic successful-store helper. Provider authentication uses OpenClaw's existing Cerebras
credential; no provider credential is written to the pinned config, prompt, report, or MCP activity log.

Recorded Android evidence from the clean-cache command:

- Candidate: [`evidence/explore-candidate.yaml`](evidence/explore-candidate.yaml).
- MCP and lifecycle activity: [`evidence/explore-mcp-activity.jsonl`](evidence/explore-mcp-activity.jsonl)
  and [`evidence/explore-activity.jsonl`](evidence/explore-activity.jsonl).
- Independent passing report: [`evidence/explore-android-report.json`](evidence/explore-android-report.json),
  with 13 MCP calls, 80,598 ms exploration, 26,406 ms verification wrapper time, 15 OpenClaw assistant
  turns, a 26,404 ms runner duration, exit code 0, and atomic persistence at
  `2026-11-22T04:58:59.928Z`.
- Stored metadata: [`evidence/explore-stored-memory.json`](evidence/explore-stored-memory.json), including
  flow SHA-256 `064e0621edf6c3e538fc717d71fca1395dd4cf32f09ec1e8a87f002959748f08`.
- Model instrumentation: [`evidence/explore-model-metrics.json`](evidence/explore-model-metrics.json).
  Usage/cost data was unavailable and is not inferred.

A deterministic invalid-candidate fixture exercises the same CLI without another model call:

```sh
WALKCACHE_OPENCLAW="$PWD/../../packages/agent-openclaw/test/fixtures/fake-openclaw.cjs" \
WALKCACHE_FAKE_OPENCLAW_MODE=invalid WALKCACHE_FAKE_OPENCLAW_NO_TURNS=1 \
  bun scripts/walkcache.ts explore checkout --config explore.config.json
```

Its asserted `Order #WC-9999` failed real WALK-002 verification (exit 1; wrapper 22,631 ms; runner
22,630 ms). The fake explorer makes no MCP or model calls, so the report records zero MCP calls and
`modelCalls: null` / `modelCallBasis: "unknown"` rather than treating the fixture as Kimi evidence.
[`evidence/explore-rejected-report.json`](evidence/explore-rejected-report.json), the
[`rejected candidate`](evidence/explore-rejected-candidate.yaml), and
[`preservation proof`](evidence/explore-rejection-preservation.json) show that successful flow and memory
SHA-256 values were byte-for-byte unchanged.

### Conservative local storage (WALK-004)

[`packages/store-local`](../../packages/store-local/README.md) now owns lookup, verified persistence,
and validation history. New successful flows are content-addressed; lookup additionally requires the
flow schema and recomputed flow checksum, and a failed latest validation is not a reusable hit. Valid
schema-1 WALK-003 metadata upgrades without inventing success. Malformed or unsafe memory fails closed;
temporary/unreferenced files never become hits, and failed replacement preserves known-good bytes.
The linked store README documents quarantine plus cold `explore` recovery and the emulator-free
regression command. Existing curated WALK-001/002/003 evidence remains unchanged.

### Comparable cold/warm metrics (WALK-005)

[`evidence/walk005-comparison.md`](evidence/walk005-comparison.md) and
[`evidence/walk005-comparison.json`](evidence/walk005-comparison.json) contain two independent real cold
explorations and six paired warm replays against the same APK, goal, emulator, stable fixture, clearState
launch, and meaningful final outcome assertion. Every sample passed. Complete measured-command wall was
96,997 / 96,518 ms cold and 24,309–25,864 ms across the warm series. Nested runner ranges overlap:
25,215 / 25,796 ms for cold verification and 24,288–25,845 ms warm. This supports model avoidance and an
observed end-to-end wall reduction in the series, not a claim of dramatically faster Maestro execution.

The measurement wrapper preserves the existing lifecycle and adds an outer monotonic timer. A cold
measurement requires no successful cache entry; a warm measurement requires an exact hit:

```sh
# After build/install, stable fixture selection, and the cold cache reset described in the ticket:
WALKCACHE_OPENCLAW="$(command -v openclaw)" \
  bun scripts/measure-sample.ts --sample cold-01 --pair pair-1 --mode cold
bun scripts/measure-sample.ts --sample warm-01 --pair pair-1 --mode warm
bun scripts/walkcache.ts run checkout  # unchanged lifecycle remains directly runnable

# After all paired samples, regenerate curated reports and JSON/Markdown comparisons:
bun scripts/compare-metrics.ts
```

The wrapper checks the device/package/stable fixture before starting its timer and validates cold model
calls against the actual OpenClaw `assistantTurns` envelope. Raw attempts and command output stay ignored;
only secret-free structured reports and aggregate evidence are curated. The ticket links all eight raw
reports, exact start/end boundaries, quality gates, and final fixture-restoration evidence. Historical
24.824 / 24.510 second samples remain excluded from these aggregates.

### Repeatable stale-selector and product-assertion failures (WALK-006)

The fixture now has a device-local runtime switch, `walkcache_checkout_selector_stale`, controlled by
`scripts/fixture-mode.sh`. In stale mode only the existing Checkout button's resource ID is omitted;
its text, click behavior, every other view, the APK, cached YAML, and build fingerprint stay unchanged.
The switch therefore reproduces a failing **cache hit**, not a stale-build miss, without editing
ignored `.walkcache/` data or changing source between runs.

From this directory, after `. ./scripts/env.sh`, the complete deterministic Android sequence is:

```sh
./gradlew clean :app:assembleDebug
adb -s emulator-5554 install -r -g app/build/outputs/apk/debug/app-debug.apk
sh scripts/fixture-mode.sh stable
bun scripts/walkcache.ts remember checkout --flow evidence/repaired-checkout.yaml

sh scripts/fixture-mode.sh stale
bun scripts/walkcache.ts run checkout  # expected exit 1: only checkout_button is stale

sh scripts/fixture-mode.sh stable
bun scripts/walkcache.ts remember checkout --flow flows/product-regression.yaml  # expected exit 1

# Restore a passing latest validation and leave the device in stable mode.
bun scripts/walkcache.ts remember checkout --flow evidence/repaired-checkout.yaml
```

The recorded stale run was a cache hit with zero model calls, exit code 1, and exactly 16 completed
steps before `Tap on id: dev.walkcache.checkout:id/checkout_button` failed. Its raw output and report
are [`evidence/walk006-selector-android-output.log`](evidence/walk006-selector-android-output.log) and
[`evidence/walk006-selector-android-report.json`](evidence/walk006-selector-android-report.json).
The wrong-order flow retained every proven step, reached Checkout and both confirmation-screen
assertions, and then failed only its deliberately wrong `Order #WC-9999 · Mochi sticker pack` text.
It stayed `passed: false`, exit code 1, `cacheStatus: "rejected"`, and
`failureClassification.category: "product_assertion"` with `repairEligible: false`; see
[`its raw output`](evidence/walk006-product-assertion-android-output.log) and
[`report`](evidence/walk006-product-assertion-android-report.json). No assertion was weakened or
rewritten to make it pass. The final stable replay passed in 25,968 ms with zero model calls and the
emulator remained running.

[`evidence/walk006-classification-matrix.md`](evidence/walk006-classification-matrix.md) documents
the positive/negative fixtures and conservative category boundaries. Classification is advisory only;
WALK-006 does not generate a source patch or decide repairs.

### Bounded source repair and genuine-regression guard (WALK-007)

`bun scripts/walkcache.ts repair checkout --config repair.config.json` now implements only the two-file
Checkout source patch. It requires the exact WALK-006 selector failure at
`dev.walkcache.checkout:id/checkout_button`; navigation, product assertions, unknown output, other
selectors, timeouts, and runner errors are refused. The complete deterministic safety contract is in
[`packages/repair/README.md`](../../packages/repair/README.md).

The accepted source-repair run used a passing cached walk created from the current stable source, then
temporarily reversed only [`evidence/walk007-repair.diff`](evidence/walk007-repair.diff) with
`git apply -R` and rebuilt/installed that stale fixture. From this directory:

```sh
. ./scripts/env.sh
./gradlew clean :app:assembleDebug
adb -s emulator-5554 install -r -g app/build/outputs/apk/debug/app-debug.apk
sh scripts/fixture-mode.sh stable
bun scripts/walkcache.ts remember checkout --flow evidence/repaired-checkout.yaml

# Experiment-only mutation; never commit this reversal. The bounded repair restores these bytes.
git apply -R evidence/walk007-repair.diff
./gradlew clean :app:assembleDebug
adb -s emulator-5554 install -r -g app/build/outputs/apk/debug/app-debug.apk
WALKCACHE_REPAIR_TIMEOUT_MS=240000 \
  bun scripts/walkcache.ts repair checkout --config repair.config.json
bun scripts/walkcache.ts run checkout
```

The [actual repair report](evidence/walk007-repair-report.json) and [exact proposed diff](evidence/walk007-repair.diff)
record one attempt inside the 240,000 ms total budget: selector-only diagnosis failed after 16 passing
steps in 39,408 ms, Gradle plus adb install passed in 911 ms, complete independent verification passed
in 25,601 ms, and atomic persistence followed. Overall lifecycle time was 65,935 ms. APK SHA-256 changed
from stale `b991a1796d81f390d9b9fd00853986fe066431bdc8f9fcb5dfce78d755e3f91a` to repaired
`1de8864ea046b64d124ece93ca97b0ae69d11d1fc3d39a89ff405f0e2dd2262d`. The unchanged flow SHA-256 is
`e09660f45502b720f79f95a65c78ad5f48a4e11ae1ff5f52c6cb99972b12b478`.
[Diagnosis](evidence/walk007-repair-diagnosis-output.log), [build/install](evidence/walk007-repair-build-output.log),
[verification](evidence/walk007-repair-verification-output.log), [previous memory](evidence/walk007-repair-previous-memory.json),
and [atomically replaced metadata](evidence/walk007-repair-stored-memory.json) are retained. A subsequent
[zero-model warm hit](evidence/walk007-repair-warm-report.json) passed in 24,129 ms.

The negative fixture is an actual app-behavior switch, not a wrong test expectation.
`sh scripts/fixture-mode.sh product-regression` makes the confirmation screen display **WC-9999** while
all selectors and checkout behavior remain unchanged. The original flow still requires **WC-1001**:

```sh
sh scripts/fixture-mode.sh product-regression
WALKCACHE_REPAIR_TIMEOUT_MS=240000 \
  bun scripts/walkcache.ts repair checkout --config repair.config.json  # expected exit 1
sh scripts/fixture-mode.sh stable
bun scripts/walkcache.ts run checkout
```

[The negative report](evidence/walk007-negative-report.json) exited 1 after 41,876 ms with
`product_assertion`, zero repair attempts, no proposed patch, no build, and no verification/replacement.
[Its raw diagnosis](evidence/walk007-negative-diagnosis-output.log) completed Checkout and failed only
at the unchanged WC-1001 assertion. [Byte-preservation evidence](evidence/walk007-negative-preservation.json)
records equal before/after SHA-256 values for memory and flow, plus a
[zero-byte app-source diff](evidence/walk007-negative-source.diff). The [restored stable replay](evidence/walk007-restored-stable-report.json)
passed in 25,653 ms; [fixture restoration](evidence/walk007-fixture-restored.txt) confirms both device
settings are `null`, the APK fingerprint is unchanged, and the emulator remains `device`.

This is narrow, deterministic repair evidence, not generalized autonomous repair. The final experiment
restored the two selector files to their pre-experiment repaired bytes; only the intentional product
regression fixture is a source change in this ticket. No repair was committed automatically and no PR
was created by the repair lifecycle. WALK-001's curated evidence remains untouched.

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

These bullets and `evidence/run-summary.json` are retained as WALK-001 historical evidence. Use the
WALK-006 runtime-switch procedure above for repeatable invalidation; do not edit ignored memory.

## What this validates

An OpenClaw/Kimi agent can learn an Android QA journey through Maestro MCP; a local learning layer
can verify and persist that walk; later runs can bypass the model entirely; and a minimal selector
repair can be verified before the cache is replaced.

## Known limitations / next production step

- The original manual cold session's total duration/model-call count remains unknown and historical.
  WALK-005 separately provides the comparable series linked above; its observed wall delta is not a
  universal speedup guarantee. `modelCalls` means OpenClaw `assistantTurns` (completed assistant/provider
  round trips), not network retries, tokens, or cost. MCP counts cover only proxied `tools/call` requests.
- Cache lookup is exact/local across normalized goal, app, platform, flow schema/checksum, and build.
  Semantic retrieval, route graphs, and subflow composition are future work.
- Classification covers only the narrow deterministic runner-output shapes documented above. It does
  not prove every root cause, and no autonomous repair loop is claimed. Bounded repair remains WALK-007.
- Evidence is Android emulator only. Web and iOS have not been attempted by this spike.
- The production step is to rewrite this proven path behind small runner/store/explorer interfaces,
  then connect cold exploration to the same CLI lifecycle without weakening verification gates.

## Reproduction commands

```sh
cd spikes/001-android-walkcache
. ./scripts/env.sh
./gradlew :app:assembleDebug
adb -s emulator-5554 install -r -g app/build/outputs/apk/debug/app-debug.apk

# Self-contained cold exploration after the build/install steps above:
WALKCACHE_OPENCLAW="$(command -v openclaw)" \
  bun scripts/walkcache.ts explore checkout --config explore.config.json

# Original single-call MCP bridge retained for spike diagnosis:
bun scripts/mcp-call.ts inspect_screen '{"device_id":"emulator-5554"}'

# Independently verify/store an existing candidate, then run a matching cache hit:
bun scripts/walkcache.ts remember checkout --flow .walkcache/candidate/checkout-android.yaml
bun scripts/walkcache.ts run checkout
cat .walkcache/reports/latest.json
```

The final checked-in fixture includes the WALK-001 repair. Its historical manual source-edit
instructions are superseded by the WALK-006 `fixture-mode.sh` runtime switch above; the original
curated diff and summary remain unchanged.

