# Déjà reference

Long reference material moved from the README.

## Verified support and limitations

| Platform | Verified behavior | Not supported or not claimed |
| --- | --- | --- |
| Android emulator, API 34, Maestro 2.12.0 | Cold exploration, independent verification/`remember`, exact warm replay, build invalidation, one bounded Checkout selector repair, and genuine product-regression handling | General-purpose repair, other apps/selectors without a new policy, physical devices, or other Android versions |
| Chromium Desktop via Maestro Web 2.12.0 **Beta/Experimental** | Independently reviewed YAML via `remember`, exact warm replay, static-build invalidation, and product-regression handling on the loopback fixture | **Cold exploration and repair are unsupported.** Firefox, Safari, mobile web, non-loopback targets, and parallel sharding are not claimed |
| iPhone 17 simulator, iOS 27.0, Xcode 27.2, Maestro 2.12.0 | Independently reviewed YAML via `remember`, exact warm replay, native `.app` build invalidation, and product-regression handling | **Cold exploration and repair are unsupported.** Physical devices, other runtimes/models, and other Maestro versions are not claimed |

Shared Maestro YAML does not establish cross-platform support. See the
[Android evidence](../spikes/001-android-deja/README.md),
[Chromium fixture and command boundary](../spikes/002-web-deja/README.md),
[native iOS fixture and command boundary](../spikes/003-ios-deja/README.md), and
the WALK-010/WALK-011 evidence files in those folders for the evidence behind this matrix. The
post-WALK-011 Android compatibility attempt was blocked before boot by insufficient emulator disk
space ([its record](../spikes/003-ios-deja/evidence/walk011-android-compatibility.txt)).

**No measured wall-clock speedup is claimed for the current CLI.** Warm replay eliminates model
calls; it does not make Maestro itself instant. Historical WALK-005 spike samples measured complete
cold commands at 96.5–97.0 seconds and warm commands at 24.3–25.9 seconds, while nested Maestro
runner ranges overlapped. Those historical spike measurements are not a current end-to-end CLI
benchmark or a performance guarantee. Current reports record per-run durations and metric scope;
compare like-for-like lifecycles before drawing timing conclusions. See
[the historical comparison](../spikes/001-android-deja/evidence/walk005-comparison.md).

## CLI

From a configured fixture directory, use `bun ../../apps/cli/deja.ts …`; from the repository
root, use `bun run deja -- … --config <path>`. Help works without configuration:

```sh
bun run deja -- --help
```

| Command | Behavior |
| --- | --- |
| `explore <goal>` | Force Android cold exploration; independently verify and persist only a passing candidate. Web and iOS return `exploration-rejected` with zero model calls. |
| `run <goal> [--rerun-failed]` | Compute exact identity and check memory first. A hit replays YAML; a normal miss explores; eligible Android stale-build memory enters one bounded repair; iOS with `staleBuild: "replay-last-verified"` replays the last verified YAML instead. A failed hit is recorded and is not reusable. |
| `remember <goal> --flow <candidate.yaml>` | Independently execute reviewed YAML and persist only after a complete pass. A bad product assertion remains a failure. |
| `migrate [--dry-run]` | With a manifest: move the old store's walks (memory schema 3) into your own files plus manifest entries. See [Consumer-owned flows](#consumer-owned-flows-and-the-walk-manifest). |
| `gate --goals <file>` | Read-only release gate: did every listed goal pass on this exact build? See [Release gate](#release-gate). |
| `list` | List valid local walks; does not invoke a runner, explorer, or model. |
| `inspect <report>` | Print a saved, redacted report. Inspecting a failed result returns its original failure class. |
| `mcp [--store <id>]` | Serve saved walks to a coding agent over MCP on stdio. See [MCP server](#mcp-server). |

Global options are `--config <file>` (default `./deja.config.json`), `--json`, and
`-h`/`--help`. `--verify` (`run`) and `--accept-assertion-change` (`remember`) need a manifest. `--rerun-failed` (`run`) is described in [Rerun a failed walk](#rerun-a-failed-walk-flaky-and-crash-results). `--store <id>` keeps this run's history in its own store (see
[One store per device](#one-store-per-device)). `--flow` is valid only with `remember`. `--junit <file>` (`explore`, `run`,
`remember`) and `--summary` (those three and `inspect`) are described in
[JUnit and summary output](#junit-and-summary-output-for-ci-hosts). Goals may contain multiple words; quote them
as one shell argument.

### Engine

Déjà owns the Maestro that runs your flows, so a host depends on Déjà only. Each Déjà release pins
one Maestro version and the sha256 of its release zip (`MAESTRO_ENGINE_PIN` in
`packages/maestro/src/engine.ts`): 2.12.0, zip sha256
`a6044a84542f522f434c5dc3d609e60c7446d423f3ed1b2ae513f5c48663ec5e`. A new pin is a Déjà release with
iOS evidence; nothing upgrades silently.

```text
$DEJA_HOME/maestro/<version>/maestro.zip            kept, so the checksum can be re-checked
$DEJA_HOME/maestro/<version>/maestro/bin/maestro    the executable
```

`DEJA_HOME` defaults to `~/.local/share/deja`. Déjà never reads or writes `~/.maestro` or any other
Maestro install, so other tools on the machine keep their own version, even while they run.

#### Runtime home

Maestro keeps its state under the JVM `user.home`: `.maestro/sessions` (a heartbeat file shared by
every Maestro process, which also prunes it), `.maestro/deps` (unpacked `applesimutils` and
`simulator-server`), `.maestro/analytics.json`, `.maestro/tests`, `.maestro/promotion-state.json`,
`.maestro/analyze-notification.json`, `Library/Logs/maestro`, `.android` (adb keys) and, for Cloud,
`.mobiledev/authtoken`. `XDG_STATE_HOME` moves only part of it (not `sessions` or `deps`), so Déjà
sets `user.home` instead.

Every Maestro process Déjà starts (`maestro test`, `maestro hierarchy`, `maestro mcp`, the
`--version` probe, `deja doctor`, and the Maestro MCP proxy environment of the OpenClaw explorer)
runs with `JAVA_TOOL_OPTIONS=-Duser.home=<runtime home>`, built by `maestroChildEnv` in
`packages/maestro/src/runtime-home.ts`:

```text
$DEJA_HOME/runtime/<version>/     the pinned engine (for example runtime/2.12.0)
$DEJA_HOME/runtime/override/      any override engine (maestro.executable or DEJA_MAESTRO)
```

- An existing `JAVA_TOOL_OPTIONS` is kept and ours is appended last. A path with spaces is quoted.
- `HOME` is not changed, so `xcrun`, `simctl`, Xcode and the user's simulators work as before. The
  device is chosen the same way.
- An override engine is isolated too: Déjà never writes to `~/.maestro`, whichever Maestro runs. All
  overrides share `runtime/override`.
- `<runtime home>/Library/Logs/DiagnosticReports` is a link to the real folder, so Maestro's iOS
  crash detection still finds reports. Déjà only reads through it.
- Maestro's analytics and the analysis notice stay off (`MAESTRO_CLI_NO_ANALYTICS=1`,
  `MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED=true`).
- The first run in a new runtime home unpacks `deps` and creates the adb key, so it is a few seconds
  slower (about 7 s measured on an iPhone 17 simulator: 26 s, then 19 s for `maestro hierarchy`).
  The iOS driver is not cached under home: Maestro bundles it and installs it on the simulator.
- A Maestro login (`maestro login`) and Cloud token in the real home are not visible to Déjà's
  Maestro. Déjà runs local flows only.
- Delete `$DEJA_HOME/runtime/<version>` at any time. It only holds Maestro's own state and logs.

- `deja engine install` downloads the pinned release from Maestro's GitHub releases to a temp
  folder, checks the sha256 (a mismatch fails closed), rejects archive paths that could leave the
  folder (zip-slip, links), extracts with the system `unzip`, and renames the folder into place. A
  failure leaves no partial folder. A folder that already matches the pin is skipped with no
  download. A folder that does not match is never replaced: install stops and names it.
- `deja doctor [--json]` reports the engine (version, path, pinned or override), Java (17 or newer),
  Xcode and the iOS simulator runtimes (read-only; it boots nothing), and whether each matches the
  tested range. Exit 1 if the engine is missing or out of range, or Java is too old. Xcode and
  runtime differences only warn.
- Runs use the pinned engine. `maestro.executable`, then `DEJA_MAESTRO`, override it; the report
  (`engine.source: "override"`, `engine.overrideVia`) and the text output name the override. A run
  without an install is exit 2 with the fix.
- Range: the tested range is `>=2.12.0 <2.13.0`. The pinned engine is not executed (its version is
  the pin). An override is asked for `--version`; outside the range, or with no readable version, the
  run is refused (exit 2) unless `maestro.allowUntested` is `true`, which warns and records
  `engine.inRange: false`. An override that cannot start fails as a launch failure (exit 1).
- Reports carry `engine: { version, source, overrideVia?, inRange }` (optional, additive). The
  summary line carries `engine=<x.y.z|unknown>`.

### Configuration

The checked-in [Android config](../spikes/001-android-deja/deja.config.json) and
[Chromium config](../spikes/002-web-deja/deja.config.json) are complete fixture examples.
Paths are resolved from the config/workspace as described below; use separate configs for separate
workspaces.

| Field | Meaning |
| --- | --- |
| `workspaceRoot` | Canonical workspace boundary, relative to the config file. `.deja/` lives here. |
| `appId`, `platform`, `buildPath`, `device` | App/platform identity, APK or static tree to fingerprint, and execution target. |
| `openClawModel`, `openClawBaseUrl`, `explorationTimeoutSeconds` | Android cold-exploration model/provider and bound. |
| `verificationTimeoutMs`, `maestro.timeoutMs` | Independent verification and Maestro execution limits in milliseconds. |
| `fixtureCredentials` | Explicit deterministic fixture values only; never real user credentials. Values are redacted from CLI output/reports. |
| Optional `maestro.executable` | Override of the pinned Maestro (see [Engine](#engine)). Omit it to use the pinned engine. |
| Optional `maestro.allowUntested` | `true` runs an override outside the tested Maestro range with a warning instead of refusing. Default `false`. |
| `openClaw.executable`, `openClaw.mcpProxy` | Executable name and the workspace-local Android MCP proxy. |
| `repair.sourceRoots`, `repair.permittedFileTypes`, `repair.affectedFiles`, `repair.timeBudgetMs` | Closed Android repair allowlist and one shared deadline. No repair block is permitted for Web. |
| Optional `crashReportGraceMs` | iOS only. Integer milliseconds, default `10000`, `0` turns it off, maximum `120000`. How long Déjà waits for a crash report after a failed attempt it labels `selector`, `navigation` or `unknown`, before the next attempt starts. See [Rerun a failed walk](#rerun-a-failed-walk-flaky-and-crash-results). |
| Optional `junit` | Path for Maestro's JUnit report, relative to the config file. Same as `--junit`; the flag wins. Off when neither is set. |
| Optional `semantic.provider`, `minimumSimilarity`, `minimumMargin` | WALK-016 opt-in local retrieval proof. `local-lexical-v1` is the only implementation; defaults are 0.90 similarity and 0.15 winner margin. Missing/unsupported providers report unavailable and use the ordinary miss lifecycle. |
| Web-only `adapter`, `browser`, `headless`, `screenSize` | Exact execution/cache identity and Chromium launch settings. Only loopback `appId` URLs and `browser: chromium` are accepted. |
| iOS `adapter`, `.app` `buildPath`, simulator-UDID `device` | Exact native bundle-tree/adapter identity and explicit `--platform ios` execution on the dedicated simulator. No repair block is permitted. |
| iOS-only optional `staleBuild` | WALK-021. Off by default (a stale build returns `repair-rejected`). `"replay-last-verified"` replays the latest verified walk's stored YAML on a new build fingerprint, after the store re-checks its checksum. A pass stores that same YAML for the new build, so the next run is an exact hit. That pass **replaces the old build's entry** (one entry per goal, app, platform, adapter and browser) and **restarts its validation and repair history** at one entry, so rolling back to the old build triggers another replay. Keeping history across builds is tracked as DEJA-12. The stored walk has `origin: "replayed-on-new-build"` (not `remember`), because only the automatic replay verified it on this build and no person reviewed it there. The store refuses the write, before storing anything, if the file's bytes differ from the replayed ones. A fail reports the real Maestro failure and stores nothing, so every later run on that build replays again (one Maestro run each); it never repairs or explores. Model calls are 0 either way, and the report's `replayedOnNewBuild` names the build the YAML was last verified on. No prior verified walk keeps `repair-rejected`. Any other value, or any other platform, is a configuration error, and core also refuses the policy when `identity.platform` is not `ios`. |
| Optional `manifest` | DEJA-36. A relative `.json` path inside `workspaceRoot`, for example `"deja.walks.json"`. Turns on consumer-owned flows: see [Consumer-owned flows](#consumer-owned-flows-and-the-walk-manifest). Not allowed with `repair`. |
| Optional `mcp.explore` | DEJA-9. `true` registers the `deja_explore` tool in `deja mcp`. Default `false`: the server is replay only. See [MCP server](#mcp-server). |
| Optional `flowsDir` | With `manifest` only. A relative folder inside `workspaceRoot`, outside `.deja/` (default `".maestro/flows/deja"`). Where cold exploration and `deja migrate` write a flow as your own file. |

When a corresponding JSON executable/timeout field is omitted, `DEJA_OPENCLAW` and
`DEJA_TIMEOUT_MS` supply defaults. `DEJA_MAESTRO` overrides the pinned engine (see [Engine](#engine)). `ANDROID_HOME`/
`ANDROID_SDK_ROOT` and `JAVA_HOME` come from the environment and are not serialized into reports.
The spike also recognizes its more specific repair override; the production config's documented
budget is `repair.timeBudgetMs`.

### Consumer-owned flows and the walk manifest

DEJA-36. With `"manifest": "deja.walks.json"` in the config, your own Maestro files are the walks.
`remember` and `run` run your flow in place, at its path in your repo, with its subflows. Déjà keeps
no copy that replay depends on, and `.deja/flows` is not read or written. The manifest is a JSON
file you commit and review like the flows:

```ts
type WalkManifest = {
  manifestVersion: 1          // an unknown version is refused
  hashVersion: 1              // version of the closure and assertion hash forms
  walks: ManifestWalk[]       // sorted, no timestamps
}
type ManifestWalk = {
  goal: string
  appId: string
  platform: 'ios' | 'android' | 'web'
  adapter: string             // for example maestro-ios/v1
  browser?: string            // web only
  flow: string                // relative to the workspace root, never under .deja/
  closureSha256: string       // the main flow plus every runFlow and runScript file
  assertionsSha256: string    // the frozen product assertions
  ownerRules?: { minAssertions?: number; requiredAssertionIds?: string[] }  // enforced on every save
  profile?: { name?: string; launchArguments?: Record<string, string | number | boolean>; clearState?: boolean }  // recorded, not read by replay yet
}
```

The manifest holds no secrets, no absolute paths and no machine data: an unknown field, an absolute
or `..` path, a path under `.deja/`, a home-path-looking value, a duplicate entry or a bad hash is
refused (exit 2). The CLI also refuses to write it if it would hold a `fixtureCredentials` value.

- **History** stays in the gitignored `.deja/` (memory schema 4): one record per goal, app,
  platform, adapter and browser, keyed by the manifest's `closureSha256`, with the build
  fingerprint and its validations. A new closure sha starts a new history. Reports, attempts and
  locks stay there too.
- **Which bytes run.** `remember` and `run` hash the closure, run your files in place, then
  hash again before they write anything. If a file moved in between, nothing is written and the
  run is `flow-changed`.
- **`flow-changed`.** If the main flow or any file it runs no longer matches the manifest's
  `closureSha256`, `run` replays nothing and exits 6. A deleted file is the same outcome with the
  miss reason `missing-flow`. It never explores.
- **`run --verify`.** Re-runs your current file in place when it changed, when no build has
  validated it (a fresh clone has the manifest but an empty `.deja/`: `not-validated`), or when the
  last run on this build failed or the build is new. A pass with an unchanged assertion hash updates
  `closureSha256` in the manifest and records the validation. If the assertion hash differs, the run
  is `assertions-changed`, exit 6, and nothing is written.
- **Changing assertions** needs a reviewed `remember <goal> --flow <file> --accept-assertion-change`.
  Without the flag, `remember` refuses a flow whose assertions differ from the manifest's.
- **Assertion hash, version 1** (not on `main` yet, so this PR defines its meaning). An ordered list
  that keeps repeats, built by walking the flow the way it runs, from the main file down. Every
  line starts with the file's path and the chain of calls that reaches it, each with its
  parameters and the blocks around it, so an assertion moved into another file, or into a subflow
  that is called under a `when`, is a different line. The lines are:
  each mapping key starting with `assert`, with its whole command mapping (canonical JSON, sorted
  keys); each `runFlow`, `retry` and `runScript` call with its parameters (`when`, `times`,
  `while`, `maxRetries`, `env`, an inline `script`) and the file it reaches; each `repeat`; every
  `evalScript` with its source and every `env` mapping, because `assertTrue` and `${VAR}`
  selectors read what they set; and the sha256 of every `runScript` file. Adding `optional:
  true`, wrapping an assertion in a condition, `times: 0`, a loosened env value, another script
  result, or swapping two assertions all change it. Not in it: actions (`tapOn` and the like), so
  an assertion moved before the action it checks is not seen; and `label`. A bare word item such
  as `- assertNoDefectsWithAi` counts like the mapping form. `extendedWaitUntil` is hashed with its
  whole mapping (`visible`, `notVisible`, `timeout`) because it fails the run when its condition is
  not met, but it does not count toward `minAssertions`. For `ownerRules`, an assertion counts as
  always running only when nothing around it is conditional or `optional` (any value but an absent
  or literal `false`, so `yes` and a `<<` merge count). Golden vectors are in
  `packages/core/test/flow-assertions.test.ts` and `packages/store-local/test/consumer-flow.test.ts`.
- **Stateless CI.** A checkout with an empty `.deja/` has no validation history, so each run says
  `not-validated` until `--verify` runs. `--verify` also accepts a changed flow, so a CI job that
  always passes `--verify` does not see `flow-changed`. Keep `.deja/` between runs, or run
  `--verify` only on the first run of a build.
- **An edit that is undone** before the save is not seen: the check is the closure hash before and
  after the run, not a copy of the files.
- **Déjà writes files and never commits.** A write to the manifest is atomic, under
  `.deja/manifest.lock` (all stores of one workspace share it), and re-checks the entry it read.
- **New-build replay** (`staleBuild: "replay-last-verified"`) runs your file at its manifest sha and
  records the new build in history. It never changes the manifest.
- **Explore writes into your tree.** A new goal (no manifest entry, no old walk) is explored cold,
  as before. The verified flow is written as a **new** file under `flowsDir` (`<goal>.yaml`, or a
  `<goal>/` folder with its layout kept when it runs subflows), and a manifest entry is added.
  Nothing is ever overwritten: a different file at that path stops the save, a flow that holds a
  `fixtureCredentials` value is never written into your tree (the check reads the raw text in its
  plain, JSON-escaped, single-quoted, URL and base64 spellings, and every decoded YAML string, so
  quotes, escapes, block scalars and flow style do not hide it; a value under 3 characters cannot be
  checked, so config refuses it), and a goal the manifest already lists is not explored. If the save
  fails after the files were written, they are removed again. Without a `flowsDir` (library hosts), explore gives
  `exploration-rejected` before any model call.
- **Repair is not available with a manifest yet** (config `repair` is refused). A repair that
  proposes a diff or a draft PR against your flow is DEJA-6.
- **Migration.** `deja migrate` (add `--dry-run` to preview, `--store <id>` for a device store):
  for each goal it takes the newest walk that last passed and whose stored copy is intact, writes
  that closure once into `flowsDir` (`<goal>/…` with its layout kept, or `<goal>.yaml`), adds the
  manifest entry with fresh hashes, and rewrites the store as memory schema 4 with the history of
  the bytes that moved. Order: files, manifest, backup, store. The old store file stays beside the
  new one as `walks.json.schema3.bak` (`walks.sqlite.schema3.bak`), copied under a temporary name and
  linked into place, so the first backup wins and a crash cannot leave a short one. `.deja/flows`
  is left alone. If a step fails, the files and manifest bytes this run wrote are put back, so a
  failed run leaves your tree as it was. Safe to run again: files and entries already there are
  not touched. Two goals whose names map to one file (`Log in`, `Log-in`) cannot both move: the
  first does, the other is left with the reason `slug-collision`, in a dry run too. A walk it cannot move
  (never passed, copy missing or changed, subflows never stored, no adapter name, a different file
  at the destination, a different manifest entry, a fixture credential value in the flow, a goal the manifest reader would refuse, a name collision) stays in the store and is listed with the reason;
  until then its goal gives `needs-migration` (exit 6). The adapter of a walk from before adapters
  were recorded (the Android adapter) comes from the config. Review the new files, then commit.
- Library: build the store with `manifest: { path }` and pass the same `manifestPath` to
  `createDeja`; a store without a manifest is refused. `run(goal, { verify })` and
  `remember(goal, path, { acceptAssertionChange })`.

### Opt-in exact-first semantic proof (WALK-016)

Semantic retrieval is off by default. Enable the deterministic local proof in a fixture config:

```json
{
  "semantic": {
    "provider": "local-lexical-v1",
    "minimumSimilarity": 0.9,
    "minimumMargin": 0.15
  }
}
```

Exact identity is always checked first and never calls or constructs an embedding provider. Only an
exact `not-found` miss enters this fallback. Candidate walks must still match app, platform, schema,
build, adapter/browser, canonical flow checksum, and latest passed validation through the local store
before they are ranked. Equivalent goals can then select one walk only above both similarity/margin
thresholds; unrelated, low-confidence, ambiguous, contradictory, and narrow adversarial-instruction
goals do not select a walk. Adversarial markers stop before cold exploration. A missing provider or
embedding failure instead follows the ordinary verified miss/explore lifecycle.
Both floors may only be raised from the defaults, never lowered/disabled. The local provider also
requires canonical-token equality after scoring so a raw hash collision cannot become a hit.

Every selected flow is revalidated immediately after provider calls and then executed by Maestro;
its result is recorded against the original stored exact identity. Semantic selection never
substitutes for verification and never broadens
repair eligibility. Human/JSON output shows semantic status/reason, embedding calls, eligible
candidates, similarity, and the selected goal/YAML separately from cold-exploration model calls.

The provider is a tiny in-process lexical hash embedder, not a learned model or remote vector DB. Its
explicit normalizations are limited to examples such as `check out`/`checkout` and `finish`/`complete`.
This proves local relevance/fallback plumbing only: **general semantic correctness, broad paraphrase
coverage, non-English understanding, and complete adversarial detection are not claimed**. See
[`packages/semantic-local/README.md`](../packages/semantic-local/README.md) and the deterministic WALK-016
tests before relying on a goal outside those examples.

### Rerun a failed walk: flaky and crash results

`deja run <goal> --rerun-failed` is off by default. When a replay fails, Déjà replays the same stored
YAML once more, on the same build, under the same timeout, and reports both attempts. The rerun makes
no model call, never explores, never repairs, never changes the YAML and never weakens an assertion.
It also applies to the iOS stale-build replay and to `--verify`.

| First attempt | Second attempt | Outcome | Exit |
| --- | --- | --- | --- |
| failed | failed, same label and same failing step | the failure as today (`failed` or `product-regression`), with both attempts | `1` |
| failed | passed | `flaky` | `7` |
| failed | failed, different label or step | `flaky` | `7` |
| failed | the flow's files changed between the attempts (manifest mode) | `flow-changed`, not rerun | `6` |

- `flaky` is an outcome, never a pass, and the host decides what to do with exit `7`. The first
  failure is recorded in the walk's history as a failure. A rerun pass is never recorded, so a flaky
  run is never a passing validation. In manifest mode the closure is re-hashed before and after the
  rerun, so the rerun only ever replays the same bytes.
- `RunResult.attempts` (optional, additive) lists both attempts in order, each with `passed`, its
  `failure` (label, failing step, raw output path, crash signal) and its artifacts. For a `flaky`
  result, `failure` is the first attempt's.
- `app_crash` and `flaky` are never repair-eligible, and a crash is never sent to repair. Labels stay
  advisory.

**`app_crash`** is set only from explicit signals, and wins over `selector` and `navigation`. A
timeout or a runner error still wins over it. Two signals count:

1. A whole line of Maestro's own output: `App crashed`, `App crashed (SIGxxx)`,
   `App crashed (signal N)`, `App crashed (unknown cause)`, or `<text> (pid N) was terminated by
   SIGxxx.` These strings are constants in the Maestro 2.12.0 jars (`maestro-client`,
   `maestro-ios-driver`). Maestro's text "The app is not running. It may have crashed or been closed."
   is not a signal. A crash-like message without a signal keeps its label.
2. iOS only: a new `.ips` crash report in `~/Library/Logs/DiagnosticReports` (a seam for tests and
   hosts: `crashReportDirectory`), written during the failed run, whose header names the app ID
   (`bundleID` or `CFBundleIdentifier`) and whose `procPath` is inside
   `CoreSimulator/Devices/<UDID>/` for the run's own simulator UDID (the config's `device`,
   compared case-insensitively). Déjà reads a bounded head of the file to match the app ID
   and keeps only its file name and process name (`failure.crash.reportFile`, `.process`). It never
   copies the contents, which can hold personal data.

Limits of the labels, which stay advisory:

- A crash report can land a few seconds after the process dies, so after a failed iOS attempt that
  Déjà labels `selector`, `navigation` or `unknown` it waits up to 10 s for the report before the
  next attempt starts. The wait applies to every such failed iOS run, with or without
  `--rerun-failed`, and costs the full 10 s when there is no crash. A timeout, a runner error and a
  failed assertion never wait. Set the config field `crashReportGraceMs` (integer, default `10000`,
  `0` turns the wait off, maximum `120000`). The wait is outside `maestro.timeoutMs` and nothing
  can abort it, so one attempt can take the timeout plus the grace period. A report is attributed
  to the attempt whose window it falls in.
- A crash followed by a failed assertion can still be labeled `product_assertion`: the report is
  still being written when Maestro prints the assertion, and no wait happens. The first-launch
  fixture run must check which step fails after the crash.
- The `.ips` match is by app ID, time and simulator. Every simulator on the Mac writes to the one
  folder, so a report counts only when its `procPath` names the run's own UDID. A report with a
  different UDID, with no UDID (for example a host process such as the Swift Testing helper), or
  with `procPath` past the 32 KB head is ignored, and Déjà writes one line to stderr:
  `deja: crash report ignored: other simulator (<file>)`, `... no simulator in report ...` or
  `... run simulator unknown ...`. Parallel runs of the same app on other simulators no longer
  give a false `app_crash`. If the run's device is not a simulator UDID (for example `booted`),
  no `.ips` report is accepted at all: the label never comes from a report it cannot attribute,
  and Maestro's own crash lines still count. Parallel runs on the same simulator are not separated.
- Déjà skips a crash report that is a symlink. It opens the file with `O_NOFOLLOW`. It also skips
  a report over 8 MB.
- The 2.12.0 sources show no prefix or format on those lines that a flow cannot produce (not seen in live output). A flow whose own
  output (for example `echo`) is exactly one of those lines can spoof the label. The cost is a wrong
  advisory label, never a repair.

Not yet recorded on a real simulator: see `spikes/003-ios-deja/README.md` for the first-launch crash
mode that will produce the evidence.

### JUnit and summary output for CI hosts

Both are opt-in. Without them, nothing changes. Exit codes are unchanged.

**`--junit <file>`** (or config `junit`) passes `--format=JUNIT --output=<file>` to every Maestro
run of the command. The file is Maestro's own report, with Déjà's redaction applied. If one command runs
Maestro more than once (cold exploration plus verification, or a failed replay then a repair), each
run overwrites the file, so it holds the last run's report. The path is added to the `RunResult`
`artifacts` as `{ "kind": "junit" }`, workspace-relative when inside the workspace.

A host must never read a missing or old file as a pass. **Hosts must treat a missing file as a
failure.** Déjà keeps that rule easy:

1. It deletes any file at that path before the command starts and creates its folder.
2. After the run, if Maestro wrote a report and either the result passed or the report shows a
   failure, Maestro's file is kept, with Déjà's redaction applied (below).
   After a `--rerun-failed` rerun the file would hold only the second attempt, so Déjà writes its own
   report with one `<testcase>` per attempt (`<goal> (attempt 1)` and `(attempt 2)`). A failed attempt
   has `status="ERROR"` and a `<failure>` with the label, failing step and output path. A `flaky` run
   always has at least one failed case.
3. Otherwise Déjà writes a minimal report. This happens when Maestro never ran (cache miss
   refused, stale build with no repairer, rejected goal, invalid memory, usage or config error
   after the command was parsed), when Maestro could not launch or was killed before writing, and
   when the result failed but Maestro's report shows a pass (for example a candidate that passed
   verification but was rejected). The file has one `<testcase>` named for the goal with
   `status="ERROR"` and a `<failure message="...">` holding the outcome, failure category and
   errors (redacted). A passing result with no Maestro report gets a `status="SUCCESS"` case. Every
   such file has `<property name="source" value="deja-synthesized"/>`; it is Déjà's result, not
   Maestro evidence.

Limits that leave no fresh file (a missing or old file still means failure):

- A path from the config field is known only after the config loads. If the config is missing or
  invalid, nothing is deleted or written and an older file stays. Pass `--junit` on the command line
  when the host needs the file replaced on every failure.
- If Déjà or Maestro is killed (SIGKILL, power loss), the old file is already deleted and no new
  one is written.

**Redaction.** Before the run returns, Déjà redacts the text a person can read in Maestro's file:
attribute values, text nodes, CDATA and comments, with entities decoded first (`&amp;`, `&lt;`,
`&quot;`, `&apos;`, numeric forms), so an escaped fixture value cannot slip through. Redacted text is
escaped again, so the XML stays well formed. Element and attribute names and the `status`,
`failures`, `errors`, `tests`, `skipped` and `time` attributes are never changed. Redaction knows
the workspace and home paths and configured fixture values only: do not publish the file as a
public CI artifact if flows contain other personal data.

The `status` values match Maestro's, so a host that treats any `<testcase>` with a status other
than `SUCCESS` as failing (as Blankaday's `maestro-main.sh` does) works with both.

**`--summary`** prints one line per run, last on stdout (with `--json` it goes to stderr, so stdout
stays one JSON document):

```text
deja-summary v=1 outcome=passed cache=hit fingerprint=3f9a1c20b7de engine=2.12.0 model_calls=0 failure=none goal=checkout
```

Grammar (one physical line, fields in this order, single spaces between them):

```text
line        = "deja-summary v=1 outcome=" outcome " cache=" cache " fingerprint=" fingerprint
              " engine=" engine " model_calls=" calls " failure=" failure [rerun] " goal=" goal
outcome     = 1*( %x61-7A / "-" )              ; a run outcome, or "error" when no result exists
cache       = "hit" / "miss" / "not-checked" / "unknown"
fingerprint = 12LOWHEX / "unknown"             ; first 12 hex characters of the build fingerprint
engine      = 1*DIGIT "." 1*DIGIT "." 1*DIGIT / "unknown"   ; the Maestro that ran the flows
calls       = 1*DIGIT / "unknown"
failure     = 1*( %x61-7A / "_" )              ; selector, navigation, product_assertion, runner,
                                               ; timeout, app_crash, unknown, unclassified, none, or error
rerun       = " attempts=2 attempt1=" label " attempt2=" label
              [" attempt1_step=" enc] [" attempt1_artifact=" enc]
              [" attempt2_step=" enc] [" attempt2_artifact=" enc]   ; only after a --rerun-failed rerun
label       = 1*32( %x61-7A / "_" )            ; that attempt's failure label, or "none" if it passed
enc         = *( ALPHA / DIGIT / "." / "_" / "~" / "-" / "%" 2HEXDIG )   ; redacted, at most 200 chars
goal        = *( ALPHA / DIGIT / "." / "_" / "~" / "-" / "%" 2HEXDIG )
```

`goal` is always last. It is the redacted, normalized goal as UTF-8, with every byte outside
`[A-Za-z0-9._~-]` percent-encoded. A goal therefore cannot contain a space, `=`, a quote, a newline,
U+0085, U+2028 or any other control or separator character, and cannot add or change a field or a
line. All other fields have the fixed charsets above. Parse the whole line with the grammar; the
text after `goal=` is the goal. Example:

```python
import re, urllib.parse
m = re.fullmatch(r"deja-summary v=1 outcome=([a-z-]+) cache=([a-z-]+) fingerprint=([0-9a-f]{12}|unknown) "
                 r"engine=(\d+\.\d+\.\d+|unknown) model_calls=(\d+|unknown) failure=([a-z_]+) goal=([A-Za-z0-9._~%-]*)", line)
outcome, goal = m[1], urllib.parse.unquote(m[7])
```

The `rerun` keys appear only when a rerun happened, so every other line is unchanged. A step or
path is redacted, cut to 200 characters and percent-encoded like `goal`, and a label that is not
`[a-z_]{1,32}` prints as `unclassified`. A flaky run reads
`outcome=flaky ... failure=app_crash attempts=2 attempt1=app_crash attempt2=none ... goal=checkout`.

`v=1` changes only if a field is renamed, removed or its charset changes. New keys may be added
before `goal`, which stays last, so parse by key (or with the whole grammar) and ignore keys you do
not know. `engine` was added this way. It is `unknown` for a run without an engine record, such as
`inspect` of an older report, and it is always strict `x.y.z` or `unknown`, whatever a report holds. When a command
fails with exit 2 or 4 after it was parsed, the line still prints, with `outcome=error`.

### Closure errors

`closure-error` (exit `2`, summary `outcome=closure-error failure=unclassified`) means a flow's files could not
be used safely, so nothing was recorded or written. The flow was not run, unless the result carries an
`execution` (the error came after the run). Causes: a reference that is unsafe or
missing (a variable, an absolute path, a hidden file, a path outside the workspace, a flow in a cycle,
two spellings of one path, an `assertScreenshot`), a manifest or flow path that is refused, or stored
files that no longer match what the walk recorded (a subflow swapped before a new-build replay runs).
The message names the file. Before DEJA-32 the same problems were thrown out of `run`: exit `2`, and a
summary with `outcome=error`. The exit code is the same; the summary now carries the outcome.
A manifest that cannot be parsed still exits `2` with its own message and `outcome=error`. A stored
file of a classic store that changed on disk is still corrupt memory (exit `4`).

Files a flow reads are `runFlow` and `retry` files, `runScript` files and `addMedia` files (a list, or a
`files` list). Media is hashed as bytes (limit 8 MiB; flows and scripts 1 MiB). `assertScreenshot`
reads a reference image Déjà does not store, so it is refused. `takeScreenshot` and `startRecording`
write their paths and are not part of the closure.

Media files are hashed and copied as bytes, up to 8 MiB each and 32 MiB in total per closure; more is a
closure error. The fixture-value scan of a media file matches the raw byte spellings of each value
only. It cannot see text that is compressed or UTF-16, or text drawn in the pixels of an image, so
keep credentials out of media. Fixture values apply in manifest mode, where Déjà writes explore output
into your tree; a classic store has no fixture values to check.

#### Behavior changes (DEJA-32)

- `run` now returns a `closure-error` result where it used to throw `FlowClosureError` (and the
  related closure and manifest errors). A host that caught the error around `run` must read the
  outcome instead. `RunOutcome` has a new member, so an exhaustive `switch` needs a case for it.
- `findFlowReferences` now throws `FlowFileCommandError` (a `FlowReferenceError`) for `assertScreenshot`,
  a malformed `addMedia`, and `retry` with a bare value. `retry` takes a mapping, as in Maestro.
- `FlowReference` has a new `command` field, and its `kind` can be `media`.
- `CandidateSnapshot` has an optional `files` list.

### Release gate

`deja gate` answers one question for a host (for example a TestFlight upload step): did every goal on
my list pass on **this exact build**? It only reads. It never runs Maestro, explores, repairs, calls a
model, writes the store or takes the writer lock, so it can run while another `deja` is writing.

```sh
deja gate --goals goals.txt --build build/Dev.app --label "$(git rev-parse HEAD)" --evidence out/gate.json
```

| Option | Meaning |
| --- | --- |
| `--goals <file>` | Required. A plain list (one goal per line, `#` comments), a JSON array of goals, or the walk manifest itself (`deja.walks.json`, whose entries name the goals). A missing file or an empty list is exit 2. |
| `--build <path>` | The build to fingerprint. Default: the config's `buildPath`. The fingerprint is a hash of the build files, so a simulator build and a store archive of one commit differ. |
| `--label <text>` | Recorded in the evidence as given, for example a commit SHA. The gate never checks it. Control characters, line breaks and format characters (bidi controls, zero-width characters) are refused (exit 2), as they are in goals, so the text table cannot be forged or reordered. |
| `--evidence <file>` | Evidence JSON path (default `.deja/gate/evidence.json`). A text table is written beside it (`.txt`). An older file is deleted first, so a stale pass is never read as this run's. Only a previous gate evidence file is ever replaced (`"kind": "deja-gate-evidence"`; the text file only when the JSON beside it is valid evidence and the text is exactly its table). Files are written through a random temporary name created with O_EXCL, so a planted symlink is never followed, and a file that changed between the check and the delete (a different inode, a symlink) is not deleted. What remains: a local user who can write that folder could still swap a file inside the microseconds between the final check and the unlink or rename. Any other existing file, a symlink, the goals file, the manifest, or a path in `.deja/` outside `.deja/gate/` is refused with exit 2 and nothing is deleted. |
| `--store <id>` | Read this store, as for other commands. |

There is no option to allow a failure or skip a goal. To check fewer goals, edit the goals file.

Per goal, the latest validation on this fingerprint decides:

| Outcome | Meaning |
| --- | --- |
| `passed` | The latest validation on this fingerprint is a pass. |
| `failed` | The latest validation on this fingerprint is a failure. |
| `flaky` | That failure came from a run that passed on a rerun or failed differently (`--rerun-failed`). See below. |
| `not-run` | No validation on this fingerprint, even if older builds passed, or the goal is not in the store. |
| `flow-changed` | Manifest mode: the flow or a file it runs no longer matches the manifest's closure sha, so an earlier pass does not speak for the file on disk. |
| `invalid` | The stored walk or its flow file cannot be read or trusted (checksum mismatch, missing file). The gate could not decide this goal: exit 11, not 10. |

"Latest" is the store's own `lastValidation` for the one walk on this fingerprint: the last one written. Timestamps are recorded but never compared, so equal timestamps and clock skew cannot reorder a pass and a failure. In manifest mode only the walk of the manifest's current closure counts.

The gate exits 0 only when every goal is `passed`. A store that cannot be read, a corrupt walk and a
build that cannot be fingerprinted never pass: exit 11 with `"verdict": "error"` in the evidence.

`flaky`: a flaky run records its first failure as an ordinary failed validation and never records the
rerun pass, so the store alone cannot tell it from `failed`. The gate reads `flaky` from the run
report (outcome `flaky` whose first attempt ended at the failed validation's time, on this
fingerprint). Without that report it reads `failed`. Both close the gate.

The evidence JSON (`schemaVersion` 1) holds `verdict` (`passed`, `closed` or `error`), the build
fingerprint, the host's `label`, and per goal: `outcome`, `validatedAt` (the run time), `flowSha256`,
`closureSha256` (manifest mode), `detail` and `reportPath` (the run report that recorded the
validation, when it is still in `.deja/reports`). It goes through the same redaction as run reports.

### MCP server

`deja mcp [--config <file>] [--store <id>]` (DEJA-9) is a stdio MCP server, so a coding agent in a
worktree can run the loop: edit the app, build, call `deja_run`, read a structured result. It is
part of the CLI (`@deja-qa/cli`) rather than a separate `@deja-qa/mcp` package. It reuses the CLI's
config loader, engine pick, store and runner assembly and report redaction, and a subcommand avoids
widening the CLI's public exports. A separate package with its own `deja-mcp` bin stays possible.
Stdout carries the protocol and nothing else; warnings go to stderr. It exits when the host closes stdin.

The device, app ID, workspace and store come from the host's config and flags. No tool can name
another workspace, device or store. Tool names use underscores because the Claude API rejects dots.

| Tool | Input (strict: an unknown field is an error) | Result |
| --- | --- | --- |
| `deja_list` | none | `{ manifest?, walks: [{ goal, goalKey, appId, platform, adapter?, buildFingerprint, flowPath, lastValidation: { passed, at }, origin }] }` |
| `deja_run` | `goal` | The schema-1 `RunResult` plus `report` (the saved report's paths). Replay only. |
| `deja_inspect` | `report?` (under `.deja/reports/`, default `latest.json`) | A saved, redacted `RunResult`. |
| `deja_remember` | `goal`, `flow` (workspace-relative `.yaml` or `.yml`) | The `RunResult`. Saved only on a full pass. |
| `deja_explore` | `goal` | The `RunResult`. Registered only with `"mcp": { "explore": true }` in the config. Spends model calls. |

What the server will not do, whatever a tool call says:

- `deja_run` never explores and never repairs. A goal with no saved walk returns `exploration-rejected`
  with `cacheMissReason: "not-found"` and `modelCalls: 0`. A stale selector is reported, not patched.
  `staleBuild` from the config still applies (iOS), because it replays an already verified flow.
- No tool takes `verify`, `rerunFailed` or `acceptAssertionChange`. A `flaky` outcome and `attempts` appear in a
  `deja_run` result only if the result carries them; they are results, never `isError`. `deja_remember` keeps the CLI rule: a failing
  candidate is never saved, and a flow whose assertions changed returns `assertions-changed`. Accepting
  that change stays a person's decision on the CLI (`deja remember --accept-assertion-change`).
- `deja_remember` resolves `flow` with `realpath`. It refuses an absolute path, a path outside the
  workspace (a symlink that leaves it included), anything under `.deja/` (also through a symlink),
  a file whose name or symlink target is not `.yaml` or `.yml`, a file with more than one hard link, and a
  flow whose config document (the first YAML document, parsed with `yaml`) does not have a plain-string `appId`
  equal to the configured app ID (a web flow: a `url` on the configured origin). It fails closed: a flow that
  does not parse, repeats a key, or has no such header is refused. Only the header is checked: per-command
  app IDs such as `launchApp: {appId}` are not (DEJA-41). The canonical path is what is verified and saved. In manifest mode the store then applies the same rules as
  `deja remember`.
- Responses use the CLI's report redaction (workspace and home paths, fixture credentials, secret-like
  keys), plus the values of key-like environment variables and the shapes of common secrets. An environment variable is key-like when
  its name (split on `_`, `-`, `.` and camelCase, with trailing digits dropped) contains PASSWORD, PASSWD, SECRET, TOKEN,
  APIKEY, CREDENTIAL, PRIVATEKEY or ACCESSKEY anywhere (`PGPASSWORD`, `APITOKEN`, `TOKEN2`), or has a whole part KEY, PASS, PWD,
  AUTH, SESSION, COOKIE, PAT, JWT, SK or PRIVATE, or a part that is API, SECRET, ACCESS, PRIVATE, DB, SSH, AUTH, SIGNING, ENCRYPTION,
  MASTER or PG followed by KEY or PASS (`DBPASS`, `SSHKEY`, `PGPASS`). `KEYBOARD`, `MONKEY`, `KEYS_DIR`, `AUTHOR`, `SESSIONNAME`, `PASSAGE` and
  `COMPASS` do not match, and the shell's own `PWD` and `OLDPWD` are left out. A key-like variable always redacts its value:
  from 4 characters for password-type names (`PASSWORD`, `PASSWD`, `PASS`, `PWD`), from 8 for the others. So an ordinary word in one
  (a `KEY` set to `dvorak-layout`) is hidden too; hiding too much is the safe side. A secret in a variable whose name has none of these words is
  caught only if its value has one of the shapes below. Value shapes
  redacted anywhere: `sk-`, `sk_`, `pk-`, `pk_`, `rk-`, `rk_` keys, `ghp_`, `gho_`, `ghs_`, `ghu_`, `ghr_` and `github_pat_` tokens, `AKIA` and
  `ASIA` AWS key IDs, `xoxa-`, `xoxb-`, `xoxe-`, `xoxp-`, `xoxr-` and `xoxs-` Slack tokens, `Bearer` tokens of 20 or more characters, and the password
  in any `scheme://user:password@host` URL (only the password part, which runs to the last `@` before the path; the user may be empty).
  Every pattern is bounded and linear: a 1 MB hostile string goes through the redactor in well under 100 ms (tested). Artifact paths are relative to the workspace.
- One writer per store: a second `deja_run`, `deja_remember` or `deja_explore` while one is running is
  refused at once with `error.code` `STORE_LOCKED`. `deja_list` and `deja_inspect` never take the lock.
  Run one `deja mcp --store <id>` per device.
- The server never edits app source, commits or opens a PR. The MCP request's abort signal is passed to
  the call; as in the facade, a Maestro flow that already started is not interrupted, so a cancelled
  call can keep the device and the store lock until that flow ends (at most `maestro.timeoutMs`).

Errors that stop a tool (a locked store, a bad path, bad configuration) come back with `isError: true` and
`{ "error": { "code", "message" } }`. A failing replay is not an error: it is a result with its failure
label and failing step. A bad argument is a JSON-RPC invalid-params error.

Claude Code, in the project's `.mcp.json`, from a source checkout (the packages are not on npm yet):

```json
{
  "mcpServers": {
    "deja": {
      "command": "bun",
      "args": ["/path/to/deja/apps/cli/deja.ts", "mcp", "--config", "/path/to/project/deja.config.json"]
    }
  }
}
```

Codex, in `~/.codex/config.toml`:

```toml
[mcp_servers.deja]
command = "bun"
args = ["/path/to/deja/apps/cli/deja.ts", "mcp", "--config", "/path/to/project/deja.config.json"]
```

After the packages are published, use `bunx @deja-qa/cli mcp --config deja.config.json` instead. The
agent then sees `mcp__deja__deja_run` and its siblings. The server starts in the client's working
directory, so give an absolute `--config` path unless the client starts in the project.

### Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Successful `run`/`explore`/`remember`, `list`, `inspect` of a passing report, or help |
| `1` | Product/unknown/runner failure (including a Maestro executable-launch failure), rejected candidate, rejected/failed bounded repair, or `deja doctor` not ready |
| `2` | Usage, configuration, engine (not installed, checksum mismatch, untested version), report-inspection, or direct I/O error; also no command. Also `closure-error` (below) |
| `3` | Cold exploration was rejected or failed to produce a verified reusable walk |
| `4` | Invalid/corrupt local memory; quarantine it and recover through verified cold exploration |
| `7` | Flaky (`--rerun-failed`): the replay failed, then passed or failed differently on a rerun of the same YAML. Not a pass; the first failure is recorded |
| `6` | Manifest mode: `flow-changed`, `not-validated`, `needs-migration` or `assertions-changed`. Nothing was replayed or written. Run with `--verify`, or review the change and run `remember` |
| `10` | `gate`: the gate is closed. At least one goal is `failed`, `flaky`, `not-run` or `flow-changed` |
| `11` | `gate`: it could not decide. The store, a stored walk or flow, or the build could not be read or trusted (`invalid`), or the evidence file could not be written |
| `5` | The store is locked by another run (`STORE_LOCKED`), two runs collided taking over a stale lock (`STORE_LOCK_CONTENTION`), or the filesystem cannot hold the lock (`STORE_LOCK_UNAVAILABLE`). Use a different `--store <id>` per device; for contention, run again |

### One store per device

A store has one writer. To run several devices at once, start one `deja` per device with its own
store id, for example a simulator UDID:

```sh
deja run checkout --store "$UDID1" &   # .deja/memory/stores/<UDID1>/
deja run checkout --store "$UDID2" &
```

Ids are 1 to 128 of `[A-Za-z0-9._-]`, starting with a letter or digit. Flow files stay shared in
`.deja/flows` (content-addressed, written atomically). Without `--store` the original single store
is used. `list` and `inspect` work while a run holds the lock. Two runs on one store: the second
exits `5` before it runs anything. The lock is released on exit, SIGINT and SIGTERM; after SIGKILL
or a crash the next run takes over once the holder is provably gone. Hosts with a Bun or Node API
pass `storeId` to `LocalWalkStore` or `SQLiteWalkStore`. Details and limits:
[ARCHITECTURE.md](ARCHITECTURE.md#one-writer-per-store-one-store-per-device-deja-4).

### Artifacts and clean-state recovery

All runtime state is generated beneath the configured workspace's ignored `.deja/`:

```text
.deja/
  flows/<goal>/<platform>-<flow-sha256>.yaml  # content-addressed verified flows
  memory/walks.json                           # schema-3 exact identities and validation history
  memory/walks.json.lock                      # writer lock, present while a run can write
  memory/stores/<id>/walks.json               # the same, for `--store <id>`
  attempts/                                   # cold/remember candidates and bounded raw logs
  patches/                                    # repair diagnosis, diff, prior bytes, verification
  reports/<command>-<timestamp>-<pid>.json    # immutable per-run result
  reports/latest.json                         # atomically replaced latest result
  reports/<run-id>/                           # runner stdout/stderr/combined output
```

`.deja/` is local state, not release evidence. Preserve it for diagnosis or move the whole
directory aside as shown in the quick start; do not edit a checksum-invalid flow into a hit. If
metadata is malformed or unsafe, the CLI exits `4` without launching Maestro. Quarantine the entire
ignored directory, then run verified cold exploration. Never delete curated files under
`spikes/*/evidence/` as part of cleanup. Only reviewed, secret-free reports, logs, flows, and diffs
belong in committed evidence.

## Bring your own model

Only cold exploration uses a model. Remember, replay and repair work without OpenClaw or any model,
and replay never needs a provider key. Cold exploration runs through OpenClaw, so any model with an
OpenAI-compatible chat API and tool calls can be configured in `deja.config.json`:

```json
{
  "openClawModel": "cerebras/moonshotai-kimi-k3.1-pro-trial",
  "openClawBaseUrl": "https://api.cerebras.ai/v1"
}
```

`openClawModel` is `provider/model`, and `openClawBaseUrl` must be an `https://` URL. OpenClaw
reads the provider key from its own settings, so Déjà never stores it. Changing the model only
changes how a test is first learned. Only Kimi K3.1 Pro on Cerebras has been measured. Other models
should work but are not tested yet.

## Safety rules

- **Secrets:** use only deterministic fixture values. Raw prompts, model responses, screen content,
  YAML-bearing subprocess output, screenshots, and state artifacts remain ignored. CLI text/JSON
  redacts configured values, credential-like fields, and workspace/home absolute paths, but redaction
  is not permission to commit raw runtime state.
- **Bounded writes:** source repair accepts only canonical, non-symlinked files under configured
  roots, with permitted extensions and exact expected content. It has one attempt and one shared
  deadline. Failed patch/build/install/verification restores original source bytes.
- **No auto-commit:** repair prints and preserves its exact proposed diff. Successful source changes
  remain visible for human review; Déjà never runs Git, commits, or opens a pull request.
- **Product regression rule:** a meaningful product assertion failure is a potential product
  regression, not stale-selector evidence. Do not weaken or rewrite the assertion, patch source, or
  save a candidate to make it pass. Product, unknown, timeout, navigation, runner, and ambiguous
  failures do not enter this repair policy. A narrowly classified selector failure must independently
  pass the unchanged full journey before atomic cache replacement.

See [Architecture](ARCHITECTURE.md) for exact cache identity, memory schemas, plugin contracts,
failure classification, and atomic-write ordering.

## Demo

[`demo/deja-demo.mp4`](../demo/deja-demo.mp4) is a **58-second, 1080p captioned fallback**
built from committed real-run evidence. It covers Android cold exploration, verified YAML, exact
zero-model warm replay, bounded repair, the product-regression guard, and the explicit Chromium
support boundary. It does not claim instant replay. [`demo/SCRIPT.md`](../demo/SCRIPT.md) links every
claim to raw artifacts; [`demo/rehearsal/`](../demo/rehearsal/REVIEW.md) records the rejected 65-second
take and two complete 58-second decode/evidence rehearsals. Regenerate with
`python3 demo/generate_video.py && python3 demo/rehearse.py` (Pillow and FFmpeg required).

## Tech stack

- Bun and TypeScript
- OpenClaw
- Kimi K3.1 Pro on Cerebras (`moonshotai-kimi-k3.1-pro-trial`)
- Maestro 2.12, installed by the host (Déjà does not install or manage it yet)
- MCP SDK
- React viewer (read-only, optional)

Déjà was formerly called WalkCache.

## Development

```sh
bun install
bun run lint                  # Oxlint
bun test --timeout=10000      # deterministic unit/component/integration suite
bun run build                 # TypeScript project build + optional viewer production bundle
bun run dev -- --host 127.0.0.1 --port 4173
bun scripts/validate-release-artifacts.ts  # run in a committed, clean tree
```

[`Portable checks`](CI.md) additionally documents the GitHub Actions Linux job, its exact
clean-checkout commands and artifact scans, and why real Android/Chromium flows remain separately
evident local integration gates. CI makes no device, model, or iOS claim.

The Vite app is an **optional read-only local viewer**, not the Déjà runtime. It cannot start
runs or repair and is unnecessary for headless use. See [viewer/README.md](../viewer/README.md).
The package scripts also include `preview` for the built viewer.

## Repository guide and evidence workflow

- [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) — implementation and persistence contracts.
- [`docs/ROUTE_GRAPH.md`](ROUTE_GRAPH.md) — diagnostic-only bounded Android route graphs, exact
  WALK-020 evidence provenance, conservative screen matching, and acceptance limits.
- [`docs/SUBFLOW_COMPOSITION.md`](SUBFLOW_COMPOSITION.md) — WALK-018 typed verified login reuse,
  clean-state composed/exact replay, byte-preservation evidence, and narrow composition limits.
- [`apps/cli/README.md`](../apps/cli/README.md) — CLI adapter details and Web boundary.
- [`spikes/001-android-deja/`](../spikes/001-android-deja/README.md) — preserved Android fixture/evidence.
- [`spikes/002-web-deja/`](../spikes/002-web-deja/README.md) — bounded Chromium fixture/evidence.
- [`spikes/003-ios-deja/`](../spikes/003-ios-deja/README.md) — bounded native iOS simulator fixture/evidence.
- [`TODO.md`](../TODO.md) — standing rules. The backlog, status and dependencies are in the Linear team DEJA.
- [`docs/PROJECT_PLAN.md`](PROJECT_PLAN.md) — historical hackathon scope and plan; current verified behavior is defined by ticket evidence, not aspirations.

The backlog lives in the Linear team DEJA (open work moved there from the old repo-local `WALK-`
tickets, which stay in git history). An issue is done only when all acceptance criteria link
reproducible commands and secret-free evidence in this repo. Record exact commands and measured
values, use `unknown` when instrumentation is unavailable, and preserve prior evidence. Closing an
issue does not authorize social posts, external submission, or other publication.

Licensed under [Apache-2.0](../LICENSE).

