# Verified Maestro runner

`runMaestro` executes the validated Android Maestro command without a shell:

```ts
import { runMaestro } from './src/index.ts'

const result = await runMaestro({
  executable: process.env.DEJA_MAESTRO!,
  device: 'emulator-5554',
  platform: 'android',
  workspaceRoot: '/path/to/workspace',
  flowPath: '.deja/flows/checkout/android.yaml',
  timeoutMs: 120_000,
  androidSdkRoot: process.env.ANDROID_HOME,
  javaHome: process.env.JAVA_HOME,
  artifacts: { directory: '.deja/reports/checkout' },
})
```

The executable, device, flow, workspace, timeout, SDK, Java, and artifact locations are explicit
configuration. No Homebrew, user-home, emulator, or SDK path is fixed by the adapter. The example
above is Android. Separate Web and iOS adapters own their command flags and build identities; their
claimed behavior is limited to the real Chromium and iOS simulator evidence in WALK-010/WALK-011.
Generic interfaces alone are never treated as platform proof.

Flows must resolve to a regular file inside the canonical workspace, including after following
symlinks. Artifact directories have the same boundary. When configured, the runner writes
`stdout.log`, `stderr.log`, and combined `output.log` in that directory. These are generated run
artifacts and should live under an ignored directory such as `.deja/`.

Every result preserves separate and combined output, ISO start/end timestamps, wall-clock and
monotonic durations, exit code/signal, timeout state, pass/fail, and structured `errors`. Error
kinds are `flow-path`, `launch`, `timeout`, `execution`, and `output-artifact` (plus
`configuration` for future configuration adapters). A nonzero exit is an `execution` error; an
artifact failure can coexist with another error and never discards captured process output.

On timeout, only the detached spawned process group receives termination signals. The runner does
not invoke `adb`, emulator controls, or broad process-kill commands. Environment values are passed
to Maestro but are never serialized into its result. Callers must still treat subprocess output as
potentially sensitive and keep generated logs ignored.

Run focused tests from the repository root:

```sh
bun test packages/maestro/test/maestro-runner.test.ts --timeout=10000
```

## Verified login composition (WALK-018)

`MaestroLoginComposer` is an explicit one-segment login-plus-suffix proof, not route planning. It
independently runs the segment and validates its YAML against exact app/build identity and
step/assertion counts before a segment store write. Bounded runner observations must additionally
prove one first-step clean launch, pre-action screen preconditions, and a trailing, still-visible
postcondition; a green process without a valid trace is rejected. `compose` reads only an exact
checksum-verified segment, emits one workspace-contained relative `runFlow` reference plus new
suffix steps, executes the complete clean-state candidate with all expanded assertions and the same
reused-boundary checks, and only then calls the ordinary `WalkStore` verified-persistence API.
Before any output mutation, canonical workspace and nearest-existing-ancestor checks reject escaping
or dangling symlinks, non-directory traversal, and non-regular existing candidates; the destination
is rechecked after `mkdir`, and temporary creation is exclusive. Legitimate new nested output remains
supported. Failed execution, observation validation, or canonical output preflight performs no goal
write and cannot overwrite an external candidate.

`countFlowAssertions` follows relative `runFlow` references for complete accounting and returns `null`
on unreadable, absolute, malformed, or cyclic references. The curated WALK-018 candidate/final flow
locations are peer directories, preventing the segment path from changing when verified bytes are
stored. Existing flows without `runFlow` are counted exactly as before, and exact cache lookup is
unchanged. See [`docs/SUBFLOW_COMPOSITION.md`](../../docs/SUBFLOW_COMPOSITION.md) for executable
fixtures and preservation evidence; no emulator/simulator or real-device run is claimed.

## Advisory failure classification (WALK-006)

`classifyMaestroFailure` adds a deterministic `selector`, `navigation`, `product_assertion`,
`runner`, `timeout`, or `unknown` category, the single explicitly failed step, and the raw combined
output artifact path. It also marks every result `advisory: true`; only narrow selector/navigation
shapes are `repairEligible`. This field is an input to the later bounded-repair ticket, not an
automatic repair or generalized diagnosis.

The boundaries are deliberately conservative:

- Structured runner timeout/launch/path/artifact failures take precedence over output text.
- A selector requires one failed tap/long-press/input step and Maestro's explicit `Element not found:`
  diagnostic. Navigation requires a failed launch/open-link step and an explicit launch diagnostic.
- A product assertion requires one failed **quoted text** assertion and Maestro's explicit
  `Assertion is false:` diagnostic. ID assertions are not assumed to be product regressions.
- Mixed diagnostics, multiple failed steps, unrecognized steps, and generic nonzero exits stay
  `unknown` with `repairEligible: false`. Product assertions are also never repair-eligible.

Positive and negative raw-output fixtures cover all six categories. Passing runs return `null`.
The CLI adds the classification to run, remember, and explore-verification reports without changing
runner exit codes or pass/fail. Cache-hit failures still invoke no explorer/model and record
`modelCalls: 0`; the failed-validation store behavior remains unchanged.

```sh
bun test packages/maestro/test/failure-classification.test.ts \
  packages/maestro/test/cache-replay-classification.test.ts \
  packages/maestro/test/maestro-runner.test.ts --timeout=10000 --check
```
