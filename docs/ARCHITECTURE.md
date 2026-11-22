# WalkCache architecture

This document describes the implementation in this repository, not a proposed general framework.
The CLI and local fixture adapters are the product surface; the preserved spikes and curated evidence
are not imported by core orchestration.

## Lifecycle

```text
goal + configuration
        |
        v
platform adapter -> build fingerprint -> exact LocalWalkStore lookup
        |                                      |
        | hit                                  | miss
        v                                      v
MaestroWalkRunner                        not-found -> Android explorer
        |                                      | stale-build -> bounded repair
        |                                      | failed validation -> failed-validation handling
        v                                      v
validation history + JSON report        independent full-flow verification
                                               |
                                               v
                                  atomic verified persistence/replacement
```

`WalkCache.run` performs storage lookup before either lazy plugin factory is called. A warm hit never
constructs `OpenClawWalkExplorer`, its model/MCP client, or a repairer. Cold exploration and repair
are not concurrent fallback branches. After a repair rejection, core does not substitute blind cold
exploration.

`remember` is a separate reviewed-YAML entry point: it runs the candidate through the same runner and
persists only a passing result. All persisted flows remain ordinary independently runnable Maestro
YAML.

## Plugin boundaries

Vendor-neutral contracts are in [`packages/core/src/contracts.ts`](../packages/core/src/contracts.ts).
Core imports no Maestro, OpenClaw, React, filesystem, demo, or platform implementation.

| Contract | Responsibility | Current implementation |
| --- | --- | --- |
| `WalkStore` | Exact lookup, prior-success diagnostic lookup, verified persistence/replacement, validation history, listing | `LocalWalkStore` in `packages/store-local` |
| `WalkRunner` | Execute a flow; return timing, assertions, artifacts, exit state, and conservative failure classification | `MaestroWalkRunner` plus Android/Web/iOS execution adapters in `packages/maestro` |
| `WalkExplorer` | Cold exploration and independently verified candidate persistence | `OpenClawWalkExplorer` for Android; explicit `UnsupportedPlatformExplorer` for Web/iOS |
| `SelectorRepairer` | At most one bounded diagnosis/source-patch/verification result | `BoundedSelectorRepairer` for the configured Android Checkout policy only |
| `RunReporter` | Serialize a completed result without changing orchestration | Atomic local `JsonRunReporter` in `apps/cli` |

The execution adapters own command selection and build identity. Android fingerprints the APK and
uses its configured emulator serial. The Web adapter fingerprints every regular file and relative
path in the configured static tree, rejects symlinks/special files, selects `--platform web --device
chromium`, and supplies adapter/browser identity. The iOS adapter fingerprints relative paths and
contents of every regular file in the native `.app` tree, selects `--platform ios` with the configured
simulator UDID, and supplies adapter identity. Android install/state/source behavior is reused for
neither Web nor iOS.

The explorer receives only goal/app/device configuration and explicit fixture values. Its empty
attempt workspace excludes app source, reference flows, successful memory, filesystem/shell/web/
memory/session tools, and secrets from the provider credential chain. Only `maestro__*` tools are
allowlisted. Parent-controlled hooks clear the fixture before exploration and verification, validate
candidate constraints, execute the complete flow, and call the store only on a pass.

## Exact cache identity

Goals are normalized by trimming, lowercasing, and collapsing whitespace. A reusable hit requires an
exact match for:

- normalized goal and stored `goalKey`;
- `appId`;
- `platform`;
- flow schema (`maestro/v1`);
- build fingerprint (Android APK SHA-256, Web static-tree SHA-256, or iOS `.app`-tree SHA-256);
- adapter and browser when supplied by an adapter (required by Web); and
- a recomputed SHA-256 of the canonical stored flow, equal to its metadata checksum.

Flow paths must remain regular files beneath `.walkcache/flows` after symlink resolution. Missing
flows, stale builds, identity mismatches, unreferenced/temporary flows, and a failed latest validation
never become hits. Malformed JSON, unsupported schemas, invalid/duplicate identities, checksum
mismatches, or path escapes raise `InvalidWalkMemoryError`; entries are not silently discarded to
manufacture a hit.

`lookupPreviousSuccessful` is diagnostic-only. It can locate the earlier successful build/flow for
bounded repair but is not a warm hit and does not authorize persistence by itself.

## Memory schema

The current `.walkcache/memory/walks.json` envelope is schema **2**:

```ts
type WalkMemory = {
  schemaVersion: 2
  walks: StoredWalk[]
}

type StoredWalk = {
  goal: string                 // trimmed original goal text
  goalKey: string              // normalized exact-match key
  appId: string
  platform: string
  flowSchemaVersion: 'maestro/v1'
  buildFingerprint: string     // SHA-256 hex
  adapter?: string             // part of exact Web identity
  browser?: string             // part of exact Web identity
  flowPath: string             // workspace-relative content-addressed YAML
  flowSha256: string
  lastValidation: { passed: boolean; at: string }
  validationHistory: Array<{
    passed: boolean
    at: string
    flowSha256: string
  }>
  origin?: 'remember' | 'explore' | 'repair'
  exploration?: {
    explorationDurationMs: number | null
    verificationDurationMs: number | null
    mcpCalls: number | null
    modelCalls: number | null
    modelCallBasis: string
  }
  repairHistory?: Array<{
    attemptId: string
    patchPath: string
    previousBuildFingerprint: string
    previousFlowSha256: string
  }>
}
```

Valid schema-1 records with the legacy complete identity/checksum are read conservatively and emit
schema 2 on their next validation/persistence write. Upgrade does not turn a failed validation into
a success. Unsupported or incomplete legacy entries fail closed.

`validationHistory` is append-only. A replay failure is recorded as the latest failed validation;
flow bytes stay available for diagnosis but cannot produce a subsequent hit until independently
verified through the supported lifecycle. Repair retains validation history and previous exploration
metrics and adds provenance for the previous build/flow.

Run reports use result schema **1** (`packages/core/src/contracts.ts`). They contain command,
identity, cache status/reason, outcome, timestamps/duration, metric basis, assertions, repair count,
YAML/checksum, advisory failure, artifacts, source diffs, and errors. `durationMs` is scoped to the
reported lifecycle/execution path; cold exploration also exposes exploration and verification
submetrics. These numbers are not automatically a comparable cold/warm benchmark.

## Atomic writes and artifact boundaries

Verified flow files are content-addressed:

```text
.walkcache/flows/<normalized-goal-slug>/<platform>-<flow-sha256>.yaml
```

Persistence writes/renames the flow before atomically replacing `walks.json`. If metadata replacement
fails, the previous referenced flow and metadata remain byte-for-byte intact; an unreferenced new flow
is ignored. Temporary files left before rename are never loaded. `recordValidation` and report writes
also use a temporary file plus rename. Atomic rename provides replacement integrity on the local
filesystem, not distributed transactions or multi-writer coordination.

Reports are written under `.walkcache/reports/<command>-<time>-<pid>.json`, then `latest.json` is
atomically replaced. Runner artifacts retain separate `stdout.log`, `stderr.log`, and combined
`output.log`. Cold/remember attempts and repair evidence stay under ignored `.walkcache/attempts/`
and `.walkcache/patches/`; accepted repair attempts also preserve the exact previous metadata/flow
bytes. Raw logs and runtime state are never curated automatically.

The optional viewer accepts only valid report-declared artifacts under `.walkcache`, with canonical
path/symlink checks and response redaction. It has read-only local `GET` endpoints and no command or
write path. Core and CLI do not depend on it.

## Failure and repair rule

Maestro classification is deliberately conservative and advisory. Structured runner failures take
precedence over output text. Selector/navigation categories require recognized failed steps and
explicit diagnostics; product assertion classification requires a quoted-text assertion and explicit
assertion diagnostic. Mixed diagnostics, multiple failed steps, unrecognized output, and generic
nonzero exits remain `unknown` and are not repair-eligible.

The current Android repair policy is narrower than the classifier: it admits only the sole failing
Checkout ID tap, exact expected source content, the two configured Java/XML files, canonical
non-symlinked paths inside configured roots, and one shared timeout. It independently diagnoses the
unchanged prior successful flow before any write, rebuilds/installs, recomputes the APK identity,
and verifies the unchanged full journey. Only then does `persistVerifiedRepair` atomically replace
successful memory. Failure restores source bytes and preserves prior cache bytes.

A meaningful product assertion failure is a potential product regression. It is not repaired,
rewritten, weakened, or persisted as success. Unknown, ambiguous, infrastructure, timeout, and other
selector/navigation cases are also refused by this policy. Repair prints its exact diff and
`autoCommit: false`; no implementation invokes Git, commits a repair, or opens a pull request.

Web and iOS cold exploration return `exploration-rejected` with zero model calls and direct the
operator to reviewed YAML plus `remember`. Neither has a repair plugin: a stale static/native build
returns `repair-rejected`, never a blind Android repair or cold fallback. iOS support is limited to the
real simulator `remember`/warm-replay/invalidation/regression behavior evidenced by WALK-011.

## Metrics and secrets

Cold `modelCalls` means OpenClaw `assistantTurns` (completed assistant/provider round trips), not
physical HTTP retries, tokens, or cost. Missing instrumentation remains `null`/`unknown`. `mcpCalls`
counts proxied `tools/call` requests, excluding initialization and discovery. Warm model/MCP counts
are zero because those subprocesses are not spawned.

Historical WALK-001 and WALK-005 measurements belong to their recorded spike lifecycles and build
identities. The current CLI has no controlled comparative wall-clock benchmark; its reports expose
current per-run values without claiming a measured speedup. Nested runner time and complete command
time answer different questions and must not be mixed.

Provider authentication remains in OpenClaw's existing credential configuration. The generated
pinned OpenClaw config contains endpoint/model/tool policy, not provider secrets. Fixture values are
explicit, deterministic, and redacted from CLI output/reports; environment SDK/Java locations are not
serialized. Nevertheless, raw subprocess, model, screen, YAML, screenshot, and state artifacts can
contain sensitive content and must remain ignored unless individually reviewed and redacted.

## Development map

```text
apps/cli/                 Argument/config validation, wiring, redaction, JSON reporting
packages/core/            Vendor-neutral contracts and lifecycle orchestration
packages/store-local/     Conservative local memory and atomic persistence
packages/maestro/         Verified runner, adapters, failure classification
packages/agent-openclaw/  Android-only bounded cold exploration
packages/repair/          Narrow Android Checkout source repair
src/, viewer/             Optional read-only local report viewer
spikes/001-.../           Preserved Android fixture and curated evidence
spikes/002-.../           Chromium fixture and explicit unsupported boundaries
spikes/003-.../           Native iOS simulator fixture and reviewed-YAML/replay evidence
tickets/, TODO.md         Repo-local delivery backlog and evidence index
```

Platform support is established only by real execution evidence linked from the corresponding ticket,
never inferred from these interfaces. See the root [README](../README.md) for reproduction,
support, CLI, and cleanup instructions.
