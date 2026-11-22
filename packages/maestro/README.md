# Verified Maestro runner

`runMaestro` executes the validated Android Maestro command without a shell:

```ts
import { runMaestro } from './src/index.ts'

const result = await runMaestro({
  executable: process.env.WALKCACHE_MAESTRO!,
  device: 'emulator-5554',
  platform: 'android',
  workspaceRoot: '/path/to/workspace',
  flowPath: '.walkcache/flows/checkout/android.yaml',
  timeoutMs: 120_000,
  androidSdkRoot: process.env.ANDROID_HOME,
  javaHome: process.env.JAVA_HOME,
  artifacts: { directory: '.walkcache/reports/checkout' },
})
```

The executable, device, flow, workspace, timeout, SDK, Java, and artifact locations are explicit
configuration. No Homebrew, user-home, emulator, or SDK path is fixed by the adapter. Android is
the only platform this runner currently accepts; it makes no iOS or web support claim.

Flows must resolve to a regular file inside the canonical workspace, including after following
symlinks. Artifact directories have the same boundary. When configured, the runner writes
`stdout.log`, `stderr.log`, and combined `output.log` in that directory. These are generated run
artifacts and should live under an ignored directory such as `.walkcache/`.

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
