# Déjà architecture

This document describes the implementation in this repository, not a proposed general framework.
The CLI and local fixture adapters are the product surface; the preserved spikes and curated evidence
are not imported by core orchestration.

## Lifecycle

```text
goal + configuration
        |
        v
platform adapter -> build fingerprint -> exact WalkStore lookup
        |                                      |
        | hit                                  | opt-in not-found semantic lookup
        |                                      | other miss -> ordinary lifecycle below
        v                                      v
MaestroWalkRunner                        constrained/reverify candidates
        |                                      | hit -> replay selected YAML above
        |                                      | miss/unavailable -> ordinary lifecycle
        v                                      v
validation history + JSON report        not-found -> Android explorer
        |                                      | stale-build -> bounded repair
        |                                      | failed validation -> failure handling
        v                                      v
validation continues                   independent full-flow verification
                                               |
                                               v
                                  atomic verified persistence/replacement
```

`Deja.run` performs storage lookup before either lazy plugin factory is called. A warm hit never
constructs `OpenClawWalkExplorer`, its model/MCP client, or a repairer. Cold exploration and repair
are not concurrent fallback branches. After a repair rejection, core does not substitute blind cold
exploration.
When WALK-016 is enabled, the lazy embedding factory is also behind that exact lookup and runs only
for a `not-found` miss. Wrong-app/platform/schema/adapter/browser, stale-build, missing-flow, and
failed-validation misses never invoke the provider or ranking. A provider-free adversarial-marker
guard still stops those requests before repair/exploration when semantic mode is enabled. Semantic
replay does not broaden repair eligibility.

`remember` is a separate reviewed-YAML entry point: it runs the candidate through the same runner and
persists only a passing result. All persisted flows remain ordinary independently runnable Maestro
YAML.

## Plugin boundaries

Vendor-neutral contracts are in [`packages/core/src/contracts.ts`](../packages/core/src/contracts.ts).
Core imports no Maestro, OpenClaw, React, filesystem, demo, or platform implementation.

| Contract | Responsibility | Current implementation |
| --- | --- | --- |
| `WalkStore` | Exact lookup, prior-success diagnostic lookup, verified persistence/replacement, validation history, listing | CLI default `LocalWalkStore` in `packages/store-local`; second conforming `SQLiteWalkStore` in `packages/store-sqlite` |
| `WalkRunner` | Execute a flow; return timing, assertions, artifacts, exit state, and conservative failure classification | `MaestroWalkRunner` plus Android/Web/iOS execution adapters in `packages/maestro` |
| `WalkExplorer` | Cold exploration and independently verified candidate persistence | `OpenClawWalkExplorer` for Android; explicit `UnsupportedPlatformExplorer` (in core) for Web/iOS |
| `SelectorRepairer` | At most one bounded diagnosis/source-patch/verification result | `BoundedSelectorRepairer` for the configured Android Checkout policy only |
| `RunReporter` | Serialize a completed result without changing orchestration | Atomic local `JsonRunReporter` in `apps/cli` |
| `GoalEmbeddingProvider` | Lazily embed one goal only after exact `not-found`; report calls separately | Opt-in deterministic `LocalLexicalEmbeddingProvider` in `packages/semantic-local`; no service or vector DB |

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

Flow paths must remain regular files beneath `.deja/flows` after symlink resolution. Missing
flows, stale builds, identity mismatches, unreferenced/temporary flows, and a failed latest validation
never become hits. Malformed JSON, unsupported schemas, invalid/duplicate identities, checksum
mismatches, or path escapes raise `InvalidWalkMemoryError`; entries are not silently discarded to
manufacture a hit.

`lookupPreviousSuccessful` is diagnostic-only. It can locate the earlier successful build/flow for
bounded repair but is not a warm hit and does not authorize persistence by itself.

## Exact-first semantic proof (WALK-016)

Semantic retrieval is disabled unless the configuration contains a `semantic` object. Exact lookup
always remains first. Only its `not-found` result can invoke the lazy provider, so an exact hit makes
zero embedding calls and zero cold-exploration model calls. Existing non-`not-found` miss reasons and
their repair/failure behavior are unchanged.

The default `local-lexical-v1` provider is an in-process deterministic bag of hashed lexical features.
It recognizes only the explicitly documented normalizations (for example, `check out`/`checkout` and
`finish`/`complete`). It uses no learned model, credentials, network, remote vector database, or new
stored-memory schema. This is a small relevance/fallback proof, **not generalized semantic
correctness**. Unknown paraphrases, non-English goals, subtle intent changes, and adversarial wording
outside the narrow marker list can miss or be misread; conservative misses are preferred.
Raw hash similarity is never sufficient for this provider: an additional canonical-token equality
guard rejects lexical hash collisions such as `email`/`search`, even when their raw cosine is 1.

Before candidate goal embedding or ranking, core filters on app, platform, flow schema, build,
adapter/browser, and latest passed validation, then calls `LocalWalkStore.lookup` with the candidate's
original goal. That exact lookup rechecks all identity constraints, canonical flow containment,
missing-flow state, and the recomputed checksum. A candidate removed or invalidated between listing
and relookup is skipped. Invalid memory/checksum errors propagate; they never become an unavailable
provider or a manufactured hit.

Cosine similarity defaults require a top score of at least **0.90** and a margin of at least **0.15**
over the runner-up. No candidates, low confidence, equal/near ties, explicit negation/opposites, and
narrow adversarial-instruction markers return a miss with no selected walk. Adversarial-marker goals
stop before provider or cold-explorer construction; other semantic misses continue the ordinary
verified cold lifecycle. Missing/unsupported providers and embedding errors report
`provider-unavailable` and also continue that ordinary lifecycle.
Configuration may raise either floor up to 1 but cannot lower/disable it; zero, null, non-numeric,
NaN, and infinite values are rejected. Vector components are scale-normalized before cosine math so
large finite inputs cannot turn a hit score into NaN.

A selected semantic walk is not an automatic pass. Core executes its unchanged stored YAML through
`WalkRunner`, then records pass/fail validation against the **selected stored exact identity**, not
the requested paraphrase. After all provider awaits, it performs one final exact store lookup of the
winner immediately before invoking the runner; a changed/missing/failed winner executes nothing and
does not trigger blind exploration. Product/unknown/selector failures remain failures; semantic selector
failures do not enter bounded repair. Exploration/model metrics remain separate from the optional
additive schema-1 `semantic` report, which exposes status/reason, provider, embedding calls, duration,
candidate counts, thresholds, top/runner-up similarity, and selected goal/flow/checksum. Reports
without that optional field remain compatible.

## Diagnostic route graphs (WALK-017)

Route graphs are separate from exact walk memory and are not consulted by orchestration or storage.
The closed Android-only schema stores bounded curated resource IDs, canonical SHA-256 screen
fingerprints, app/build identity, validation timestamps, verified taps, separate failed branches, and
hashed curated-evidence provenance. It has no fields for visible text, credentials, personal data,
screenshots, raw UI dumps, or environment state. Matching reports repeated, ambiguous, changed, and
unknown observations conservatively and always returns `cacheHit: false`; unsafe/unrecognized states
fall back to the existing exact replay/exploration lifecycle rather than weakening identity.

The [route-graph guide](ROUTE_GRAPH.md) links the four-node Android graph derived solely from accepted
curated WALK-020 cold/warm evidence. No new device run is claimed. This diagnostic memory does not
make transitions independently executable or establish complete app coverage.

## Explicit verified subflow composition (WALK-018)

`subflow.ts` defines a separate closed `SubflowContract`/`VerifiedSubflowStore` contract: exact
app/build identity, typed input descriptors (never input values), screen outputs, launch/screen
conditions, bounded step/assertion counts, and hashed provenance. A Maestro-specific composer takes
one independently verified clean-launch login segment and an explicit checkout suffix. It references
the stored segment with `runFlow`; it does not copy login YAML or invoke route planning, semantic
segment selection, an explorer, or a model.

The candidate and final goal-flow directories are peers, so their relative segment reference is
proved identical and workspace-contained before execution. Canonical workspace and nearest-existing
output-ancestor validation runs before any directory/file mutation and again after `mkdir`; symlink,
dangling-link, non-directory, and non-regular-candidate attacks fail before runner invocation without
touching external files. The complete expanded candidate passes
`WalkRunner` from clean state with every reused/new assertion before
`WalkStore.persistVerifiedCandidate` can run. Bounded runner observations, not YAML alone, must prove
one first-step clean launch, pre-action screen preconditions, and the trailing postcondition
boundary. Nested subflows, duplicate login commands, second launches, containment escapes,
identity/checksum mismatches, missing observations, and mismatched contract accounting fail closed.
Segment storage is separate from goal memory and content-checks reusable YAML. Runner failure
performs no goal write; goal metadata failure preserves its old bytes and never mutates verified
segment bytes. Ordinary `Deja.run` then replays the stored composed flow under the unchanged
exact identity and zero-model lookup behavior.

See [the WALK-018 composition guide](SUBFLOW_COMPOSITION.md) for curated YAML, clean-state and failure
logs, exact WALK-020 provenance, reproduction commands, and the deliberately narrow acceptance
limits. No new device execution is claimed.

## Memory schema

The CLI-default `LocalWalkStore` uses the `.deja/memory/walks.json` envelope at schema **2**.
The same `StoredWalk` logical contract is stored relationally by the second local `SQLiteWalkStore`
in `.deja/memory/walks.sqlite` (`PRAGMA user_version = 2`), with separate ordered history tables:

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

The filesystem store reads valid schema-1 records with the legacy complete identity/checksum
conservatively and emits schema 2 on their next validation/persistence write. Upgrade does not turn a
failed validation into a success. Unsupported or incomplete legacy entries fail closed. The SQLite
store accepts only its schema 2; it does not import JSON schema 1. Both implementations validate all
identities, checksums, history, origins, and exploration/repair metadata before lookup or replacement.
Both stores also canonicalize metadata reads/writes within the workspace on every operation. They
resolve the nearest existing ancestor for not-yet-created targets without walking above the workspace,
require regular-file targets and directory traversal, and reject external symlinks and non-directory
traversal before opening, creating, or replacing metadata. SQLite performs this check before using an
already-open connection as well as before opening read/write handles.

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
.deja/flows/<normalized-goal-slug>/<platform>-<flow-sha256>.yaml
```

Both stores write/rename the flow before atomically replacing metadata. `LocalWalkStore` replaces
`walks.json` with a temporary file plus rename; `SQLiteWalkStore` replaces relational metadata in one
`BEGIN IMMEDIATE` transaction with `synchronous = FULL` and a rollback journal. If metadata
replacement fails, the previous referenced flow and logical metadata remain intact (the shared
injected-failure test also checks byte-for-byte snapshots); an unreferenced new flow is ignored.
Temporary files left before flow rename are never loaded. SQLite validation writes use the same
transaction mechanism; filesystem validation and report writes use temporary file plus rename.

Filesystem rename and SQLite commit provide local replacement integrity, not one cross-resource
transaction, distributed transactions, or multi-writer coordination. SQLite serializes immediate
writers and has a 1-second busy timeout, but its high-level read/modify/write lifecycle has no
optimistic revision check or update merge: persistence/validation writers must be single-writer per
workspace. Deterministic rename failures and a real trigger-aborted SQLite transaction are tested;
power loss, drive failures, every crash point, and concurrent-writer conflicts are not certified.
See `packages/store-sqlite/README.md` for setup and durability boundaries.

Reports are written under `.deja/reports/<command>-<time>-<pid>.json`, then `latest.json` is
atomically replaced. Runner artifacts retain separate `stdout.log`, `stderr.log`, and combined
`output.log`. Cold/remember attempts and repair evidence stay under ignored `.deja/attempts/`
and `.deja/patches/`; accepted repair attempts also preserve the exact previous metadata/flow
bytes. Raw logs and runtime state are never curated automatically.

The optional viewer accepts only valid report-declared artifacts under `.deja`, with canonical
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
packages/store-local/     CLI-default JSON memory and atomic filesystem persistence
packages/store-sqlite/    Second local store: relational metadata, transactional replacement, YAML
packages/store-conformance/ Same WalkStore behavior suite and actual warm orchestration for both
packages/semantic-local/  Opt-in deterministic lexical embeddings; no model/network/vector DB
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
