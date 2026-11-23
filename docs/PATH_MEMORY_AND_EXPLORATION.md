# Exploration, AI testing and optimization

Status: proposal. Nothing in this document is built. A part counts as working only when its ticket
in the Linear team DEJA (DEJA-18 to DEJA-23, moving to Linear team DEJA) links
real run evidence. Older `WALK-0xx` names below are the repo tickets that moved to Linear.

## 0. Scope

Déjà is a generic SDK with three goals:

1. **Explore.** Find a path through an app for a goal, with any model or with none.
2. **AI testing.** Turn a natural-language goal into a verified Maestro test with real product
   assertions.
3. **Optimization.** Spend fewer model calls and less wall time per explore and repair, and emit
   shorter flows with no duplication.

iOS and Android are both first-class. Every feature must work on both. Web (Chromium) is a distant
third: the contract allows it, and no ticket here targets it.

iOS goes first, because it is the platform this Mac can test (iPhone 17 simulator, iOS 27.2,
Maestro 2.12). The order is: the platform-neutral contract with iOS observation (DEJA-18), iOS cold
exploration of one Blankaday flow (DEJA-19), Android parity on an emulator (DEJA-20), then path memory (DEJA-21),
reuse and optimization (DEJA-22), and repair, gaps and the review queue (DEJA-23). All of them build on the
`createDeja` facade (DEJA-5).

Run history across builds is parked in the DEJA backlog. Until it lands, path memory keeps records
for the current build and the last verified build only (section 4.6).

## 1. The problem today

- **One YAML per goal.** `WalkStore` (`packages/core/src/contracts.ts:100`) maps an exact identity,
  build included, to one flow under `.deja/flows/`. Goals share nothing. Blankaday's
  `sign-in.yaml`, `delete-account.yaml` and `export.yaml` all walk Today, You and Settings, and a
  renamed `you.settings` breaks all three with no shared diagnosis.
- **The build is in the key.** Blankaday's runner builds a new `.app` each run, so a store keyed by
  build is empty every run (DEJA-1).
- **Exploration is one Android agent loop.** `packages/agent-openclaw` hands Kimi the Maestro MCP
  tools and asks for a whole YAML file back (`explorerPrompt`, `packages/agent-openclaw/src/index.ts`).
  The model writes the assertions. iOS and web return `exploration-rejected`.
- **Repair is one fixture policy.** `CheckoutRepairPolicy` fixes one Checkout ID in two files.
- **Diagnostic memory is narrow.** The WALK-017 route graph (`packages/core/src/route-graph.ts`) is
  Android-only, tap-only and per build. The WALK-018 subflow (`packages/core/src/subflow.ts`) is one
  hand-composed login.

What stays: the deliverable is ordinary Maestro YAML that the owner reviews and runs without Déjà.
`WalkStore`, memory schema 2 and the WalkStore conformance suite don't change.

## 2. Test cases from the first consumer

Blankaday's `.maestro/` (read-only here) is the test set.

| Case | Where | What the design must do |
| --- | --- | --- |
| Several clean-launch sections in one file | `today.yaml` (2), `reveal.yaml`, `sign-in.yaml` (2), `export.yaml` (2) | Treat each `launchApp clearState: true` as the root of its own path |
| Launch arguments that seed state | `uiTesting: true`, `"-settingsFriends": true`, `"-screenshotState": history`, `"-uiTestDate": "2026-10-08"`, `onboarding: true` | Make the launch profile part of the context. `screen.today` under `streak` isn't the same state as on a fresh install |
| Warm relaunch | `onboarding.yaml`: `stopApp`, then `launchApp` with no `clearState` | Keep a warm relaunch inside its section |
| Subflow with parameters | `runFlow: file: ../subflows/type.yaml`, `env: TEXT: "Movie night"` | Store the `runFlow` closure and parameters by name |
| Shared prefix | Today, You, Settings in `sign-in.yaml`, `delete-account.yaml`, `export.yaml`; `sign-in-from-settings.yaml` used twice | Diagnose a shared screen once across contexts; reuse a path only inside its context |
| Non-tap commands | `scrollUntilVisible`, `swipe`, `repeat`/`while`, `pressKey: Enter`, `eraseText`, `tapOn: point` | A step is a short command segment, not one tap |
| Screen markers | `screen.today`, `screen.settings`, `screen.reveal` | Use a marker ID as the main part of a fingerprint |
| Copy selectors | `assertVisible: "Discard changes?"`, `tapOn: "Discard"` | Report as gaps. Blankaday's rule is "select by ID, never by copy" |
| iOS system UI | `ActivityListView` (share sheet), keyboard key `space`, faked notification permission | Treat as system UI or an overlay, never as an app screen |

## 3. Platform-neutral explorer contract

### 3.1 What is shared and what is per platform

The algorithm, the data model, budgets, assertions, verification and storage are shared. A
platform adapter supplies only three things: hierarchy capture, fingerprint rules and action
execution. Each adapter goes through Maestro, so the output is always Maestro YAML.

```ts
/** One per platform. The only platform-specific code the explorer calls. */
export interface ExplorationDriver {
  readonly platform: 'ios' | 'android' | 'web'
  readonly rules: FingerprintRules
  /** Clean launch with a profile, then replay any state paths. */
  reset(profile: LaunchProfile, statePaths: Segment[][]): Promise<StepResult>
  /** Read the current screen. Never relaunches. */
  observe(): Promise<Observation>
  /** Run one segment on the live app. Never relaunches. */
  act(segment: Segment, env?: Record<string, string>): Promise<StepResult>
  /** The platform's own way back, if any (Android back, iOS nav bar back or edge swipe). */
  backActions(observation: Observation): CandidateAction[]
  /** Platform system UI the harness may handle without the model. */
  systemUi(observation: Observation): SystemUiState | null
}

export interface FingerprintRules {
  /** Platform ID to a stable token, or null to drop it (e.g. strip Android's package prefix). */
  normalizeId(raw: string): string | null
  isOverlay(element: RawElement): boolean       // keyboard, toasts
  isSystemUi(element: RawElement): boolean      // alerts, permission sheets, share sheet
}

export type Observation = {
  elements: ElementObservation[]   // normalized, bounded (at most 400)
  systemUi: SystemUiState | null
  keyboardVisible: boolean
  truncated: boolean               // true when the bound cut elements
  snapshot: { path: string; sha256: string } // stored normalized snapshot (section 4.4)
  capturedAt: string
  durationMs: number
}

export type ElementObservation = {
  id: string | null                // normalized stable ID
  role: 'button' | 'text-field' | 'secure-field' | 'cell' | 'switch' | 'tab' | 'text' | 'other'
  enabled: boolean
  interactive: boolean
  label: string | null             // kept only in memory for this run; stored only if config allows
  overlay: boolean
}

export type StepResult = {
  passed: boolean
  failure: WalkFailure | null
  durationMs: number
  crashed: boolean                 // explicit crash signal only (DEJA-7)
}
```

The explorer keeps implementing today's `WalkExplorer.explore({ identity })`
(`contracts.ts:225`). The new `GraphExplorer` takes an `ExplorationDriver`, the `WalkRunner`
interface (not `MaestroWalkRunner`), an optional `PathStore`, an `ActionChooser`, budgets and a
goal spec through `createDeja` options. `UnsupportedPlatformExplorer` stays for platforms with no
driver yet.

### 3.2 iOS and Android differences

| Concern | iOS (XCUITest through Maestro) | Android (UiAutomator through Maestro) |
| --- | --- | --- |
| Hierarchy | XCUITest accessibility snapshot. SwiftUI can merge children into one element or pass one identifier down to children. Lazy lists hold only on-screen rows. Large screens are slow to snapshot | UiAutomator dump. Compose exposes `testTag` as a resource ID only when `testTagsAsResourceId` is on; otherwise it's a gap |
| Stable ID | `accessibilityIdentifier` (Blankaday: `screen.today`, `you.settings`) | `resource-id`, normalized by dropping the `com.app:id/` prefix; `content-desc` as a fallback label, not an ID |
| Launch arguments | `launchApp arguments:` become process arguments the app reads (Blankaday: `uiTesting`, `-screenshotState`) | `launchApp arguments:` become intent extras the app must read |
| Clean state | `clearState: true` resets the app container. The keychain and any server state stay, so the app needs a test mode (Blankaday's `uiTesting` with fixture providers) | `clearState: true` or `pm clear` (as today). Server state stays |
| Going back | No system back. The nav bar back button (often labeled with the previous title, a text selector) or a left-edge swipe; sheets close with a downward swipe | The `back` command, always available. It can leave the app at the root |
| System alerts | SpringBoard alerts and permission sheets (notifications, photos, location, tracking) sit outside the app. Prefer `launchApp permissions:` or the app's fake (Blankaday's `-notificationAuth`). An unexpected alert stops the step as `system-ui` | Runtime permission dialogs from the permission controller; app dialogs are in-app. Prefer `permissions:` on launch |
| Keyboard | Software keyboard keys show up as elements (`space`), which are overlays. `pressKey: Enter` submits. On the iOS 27 simulator, `inputText` can attach a hardware keyboard and hide the software one for about 2 s (Blankaday's `subflows/type.yaml` waits for `space`) | IME keyboard. `hideKeyboard` closes it; `pressKey: Enter` submits |
| Out-of-process UI | Share sheet (`ActivityListView`), photo picker: observable in part, not explorable. The explorer stops at them and reports | Share sheet and pickers from other packages: same rule |
| Screen sizes | One layout must fit 343×667 to the Duo outer display; never use point taps from search | Many sizes; same rule |

Web, for completeness: `id` or a configured test attribute, browser back, a new browser context to
reset. No ticket here targets it.

## 4. Path memory data model

### 4.1 Principles

1. **The graph is keyed without the build.** Builds appear only on validation records.
2. **Two layers of identity.** A *screen* is keyed by its fingerprint alone, across all contexts.
   Diagnosis works at this layer. A *transition* (one segment run from one screen) is keyed by its
   context. Verification and composition work at this layer, because one screen can behave
   differently under different launch arguments.
3. **The verified unit is a path from a root.** A root is a clean launch with a profile. A
   transition is trusted only because a whole path through it passed.
4. **A segment is a short, verbatim Maestro command list**, content-addressed and context-free, so
   its ID never changes when screens are observed later.
5. **Every step carries provenance** and a pointer to the snapshot it came from.
6. **No secret values.** Parameters are stored by name and kind.
7. **Memory is an index, not a cache hit.** Only a full verified flow in `WalkStore` makes `run` a hit.

### 4.2 Types

```ts
export const PATH_MEMORY_SCHEMA_VERSION = 1
export const SCREEN_FINGERPRINT_VERSION = 2 // WALK-017 is version 1, Android only

export type GraphKey = {
  appId: string
  platform: 'ios' | 'android' | 'web'
  adapter: string             // 'maestro-ios/v1', 'maestro-android/v1'
  browser?: string
  flowSchemaVersion: 'maestro/v1'
  fingerprintVersion: typeof SCREEN_FINGERPRINT_VERSION
}

export type LaunchProfile = {
  profileId: string           // sha256 of the canonical fields below
  clearState: true
  arguments: Array<{ name: string; value: string | boolean | number | ParamRef }>
  permissions?: Record<string, 'allow' | 'deny' | 'unset'>
  explore: boolean            // owner marks the profile safe to explore (fixtures, no real server)
  label?: string
}

/** A profile plus the state-changing paths that ran (config.stateChangingPaths, e.g. sign-in). */
export type Context = {
  contextId: string           // sha256(profileId + ordered state path ids)
  profileId: string
  statePathIds: string[]
}

export type ScreenFingerprint = {
  version: typeof SCREEN_FINGERPRINT_VERSION
  marker: string | null       // e.g. 'screen.settings' from config.screenMarkerPattern
  anchors: string[]           // sorted, filtered stable IDs, at most 32
  sha256: string
}

export type ScreenNode = {
  nodeId: string              // = fingerprint.sha256
  fingerprint: ScreenFingerprint
  seenInContexts: string[]
  snapshot: SnapshotRef       // first observation
}

export type SnapshotRef = { path: string; sha256: string }

export type Provenance =
  | 'imported'                // split from an owner-approved flow
  | 'observed'                // seen during an observed replay or exploration step
  | 'model-inferred'          // the model picked it from offered actions
  | 'repaired'                // produced by graph repair
  | 'ambiguous'               // the screen match was ambiguous; never used for planning

export type ParamRef = { param: string }
export type ParamDescriptor = {
  name: string
  kind: 'text' | 'credential' | 'date' | 'fixture-choice'
  redact: boolean
  source: 'config-fixture' | 'literal-in-owner-yaml' | 'generated'
}

export type ValidationRecord = {
  buildFingerprint: string
  passed: boolean
  at: string
  runId: string
  scope: 'standalone' | 'in-parent-flow' | 'in-path'
  device?: { model: string; os: string }
  maestroVersion?: string
}

export type Segment = {
  segmentId: string           // sha256(canonical commands + closure shas)
  commands: string            // canonical YAML, may contain ${PARAM}
  closure: Array<{ path: string; sha256: string }> // runFlow/runScript files (DEJA-2)
  params: ParamDescriptor[]
  assertions: string[]        // canonical assertion commands; frozen once approved
  selectorQuality: 'id' | 'text' | 'point' | 'mixed'
}

export type Path = {
  pathId: string              // sha256(contextId + ordered segment ids)
  contextId: string
  name: string                // section name or '<flow>#<n>'
  segmentIds: string[]
  provenance: Provenance
  validations: ValidationRecord[]
}

export type Transition = {
  transitionId: string        // sha256(contextId + fromNodeId + segmentId)
  contextId: string
  fromNodeId: string
  segmentId: string
  toNodeId: string
  provenance: Provenance
  snapshots: { before: SnapshotRef; after: SnapshotRef }
  validations: ValidationRecord[]
  failedBranches: Array<{ category: string; at: string; buildFingerprint: string }>
  nondeterministic: boolean
}

export type GoalComposition = {
  goalKey: string
  pathIds: string[]
  target: GoalTarget | null
}

/** Approved product assertions. Hash checked on every replay and repair. */
export type GoalTarget = {
  source: 'owner-goal-spec' | 'owner-yaml' | 'approved-proposal'
  assertions: string[]
  assertionsSha256: string
  approvedAt: string
}

export type GraphVersion = {
  version: number
  at: string
  trigger: 'import' | 'observed-run' | 'build' | 'approved' | 'auto-merged' | 'revert'
  runId?: string
  proposalId?: string
  buildFingerprint?: string
}

export type PathGraph = {
  schemaVersion: typeof PATH_MEMORY_SCHEMA_VERSION
  key: GraphKey
  version: number
  versions: GraphVersion[]    // append-only; a revert adds a version, it never deletes one
  profiles: LaunchProfile[]
  contexts: Context[]
  segments: Segment[]
  paths: Path[]
  nodes: ScreenNode[]
  transitions: Transition[]
  goals: GoalComposition[]
  proposals: Proposal[]       // section 9.3
}
```

Derived transition status, computed on read: `verified-on-build`, `verified-elsewhere` (a
hypothesis for this build), `failed-on-build`, `stale` (a different screen at one end on this
build), `candidate` (never in a passing path), `nondeterministic`.

How the two layers work in Blankaday. `sign-in.yaml` and `delete-account.yaml` launch with
`uiTesting` and `-settingsFriends`; `export.yaml` with `uiTesting` only. All three run
`tapOn today.you`, then `tapOn you.settings`: 2 contexts, the same segments, the same screens. A
renamed `you.settings` is one finding at the screen layer for all three goals. Composition stays in
one context: a new `-settingsFriends` goal can reuse `sign-in.yaml`'s transitions and the
`sign-in-from-settings` path, but not a transition seen only under `export.yaml`'s context.

**Limit: hidden state.** In `sign-in.yaml`, `tapOn settings.account` from `screen.settings` opens
sign-in before signing in and the Account page after. A declared state-changing path keeps these
apart as two contexts. When sign-in runs inline, the same screen and segment reach two screens, so
the transition is marked `nondeterministic` and skipped. That costs reuse, not correctness.

### 4.3 Screen fingerprints (version 2)

1. Keep elements whose ID passes `rules.normalizeId`.
2. Drop overlays and system UI (`rules.isOverlay`, `rules.isSystemUi`, `config.fingerprint.ignore`).
3. Drop dynamic IDs: numeric, UUID or date-like suffixes.
4. Drop "chrome" IDs present on most screens in the graph (for example `tab.today`). This only
   removes IDs.
5. A `config.screenMarkerPattern` match (Blankaday: `^screen\.`) is the marker. Same marker with a
   subset of stored anchors is `matched-scrolled`, never a hit by itself.
6. Visible text is never part of a fingerprint. It changes per locale and can hold personal data.

Matching keeps WALK-017's rules: `ambiguous`, `changed` and `unknown` fail closed. A screen with no
stable IDs is never a node; it is a gap.

### 4.4 Snapshots

Each observation stores a normalized snapshot under `.deja/paths/<key>/snapshots/<sha>.json`:
elements with IDs, roles and flags. Labels are stored only when `config.snapshots.includeLabels` is
on. No screenshots, no raw hierarchy dumps. Every transition, gap and proposal points at the
snapshots it came from, so a person can see why.

### 4.5 What counts as verified

| Thing | Verified on build B when | Used for |
| --- | --- | --- |
| Goal flow | A full independent run from a clean root passed on B (today's rule) | `run` hits, gates |
| Path, standalone | The path alone, from its root, passed on B | Section replay, prefix for exploration |
| Path, in parent flow | The parent flow passed on B | Reporting only |
| Transition | A path containing it passed on B, and observations showed both ends | Planning and composition on B, in its context |
| Candidate transition | Seen once in exploration | A hypothesis. Never replayed as verified |

### 4.6 Records while run history is parked

Full history across builds is parked. Until it lands, each path and transition keeps at most two
record slots: the current build, and the last build where it passed. That is enough for
"verified-elsewhere" hints and stale detection. It is not enough for trends or flake rates. The
type stays `ValidationRecord[]`, so the parked item can lift the limit without a schema change.

## 5. Storage and store contract

```text
.deja/
  flows/..., memory/walks.json, memory/walks.sqlite   # unchanged WalkStore, schema 2
  paths/<graph-key-sha>/
    graph.json                     # PathGraph, schema 1
    segments/<sha>.yaml
    closure/<sha>/<relative path>
    snapshots/<sha>.json
  memory/paths.sqlite              # SQLite PathStore, own file and own user_version
```

Writes follow today's rules: content files first, then an atomic `graph.json` replacement or one
`BEGIN IMMEDIATE` transaction. Canonical-path and symlink checks match
`packages/store-local/src/index.ts`. One writer per workspace (DEJA-4).

```ts
export interface PathStore {
  graph(key: GraphKey): Promise<PathGraph | null>
  importVerifiedFlow(input: ImportFlowInput): Promise<Path[]>      // scope 'in-parent-flow'
  recordObservedRun(run: ObservedRun): Promise<GraphVersion>        // append-only
  persistVerifiedPaths(input: PersistPathsInput): Promise<GraphVersion>
  propose(proposal: NewProposal): Promise<Proposal>
  applyProposal(id: string, by: 'human' | 'auto'): Promise<GraphVersion>  // one code path
  revert(toVersion: number): Promise<GraphVersion>
  listPaths(key: GraphKey, filter?: { goalKey?: string; contextId?: string }): Promise<Path[]>
}

export type ObservedRun = {
  key: GraphKey
  buildFingerprint: string
  runId: string
  pathId: string
  passed: boolean
  steps: Array<{
    position: number               // segments can repeat in one path
    segmentId: string
    before: { fingerprint: ScreenFingerprint; snapshot: SnapshotRef } | null
    after: { fingerprint: ScreenFingerprint; snapshot: SnapshotRef } | null
    status: 'passed' | 'failed' | 'not-reached'
    failureCategory?: string
  }>
}
```

A new `runPathStoreContractTests` in `packages/store-conformance` covers: empty graph, unsupported
schema, tampered segment or closure file, escaping closure path, append-only records and versions,
build-A records never answering for build B, ambiguous fingerprints failing closed, a contradiction
proposal changing nothing, `applyProposal` by human and by auto producing the same state, and
injected write failures preserving earlier bytes. The WalkStore suite doesn't change.

**Migration from memory schema 2.** No rewrite of `walks.json` or `walks.sqlite`. A read-only
import splits each passing flow at every `launchApp` with `clearState: true` (a warm `launchApp`
and `stopApp` stay inside the section), writes profiles, segments (each closed by an assertion) and
paths with `provenance: 'imported'` and one `in-parent-flow` record. No screens or transitions are
written until an observed run sees them.

**Naming a failing section.** `failedStep` returns step text only
(`packages/maestro/src/failure-classification.ts:27-34`), and `reveal.yaml` repeats steps across
sections. The section is found by position: completed clean launches before the failing step in
Maestro's ordered output. This assumes ordered step lines with `runFlow` expanded in place, which no
raw iOS log in this repo confirms yet. Without a position, the section is `unknown`, never guessed.

## 6. Identity and versioning

| Version | Value | Changes when |
| --- | --- | --- |
| `PATH_MEMORY_SCHEMA_VERSION` | 1 | The `PathGraph` shape changes; other versions are rejected |
| `SCREEN_FINGERPRINT_VERSION` | 2 | Fingerprint rules change. It is in `GraphKey`, so a change starts a new graph |
| `PathGraph.version` | counter | Every accepted change, with its trigger |
| Adapter ID | `maestro-ios/v1`, `maestro-android/v1` | Command execution changes. Android has no adapter ID in `WalkIdentity` today (`packages/maestro/src/platform-adapter.ts:75`); path memory always records one |
| Memory schema (WalkStore), result schema | 2, 1 | Unchanged. Reports get optional additive `exploration.graph` and `metrics` sections |

Device model, OS runtime and Maestro version go on records, not in the key.

## 7. The exploration algorithm

### 7.1 Primitives

| Primitive | How | Model |
| --- | --- | --- |
| `reset(ctx)` | `driver.reset`: one-command `launchApp` flow with the profile, then state paths | No |
| `observe()` | `driver.observe`: `maestro hierarchy` (listed by `maestro --help` on Maestro 2.12) or MCP `inspect_screen`, normalized | No |
| `act(segment)` | `driver.act`: a flow with no `launchApp` on the live app | No |
| `settle()` | `waitForAnimationToEnd`, then observe until two reads give the same fingerprint (at most 5 tries); an unsettled screen is never a node | No |
| `verify(flow)` | Today's independent full run through `WalkRunner` from a clean root | No |

**Assumption, not yet proven.** No code in this repo steps a real Maestro run. The only step
protocol is the fixture's (`packages/maestro/src/execution-observations.ts:23`). DEJA-18 must show on the
iOS 27.2 simulator that a flow with no `launchApp` acts on live state, that the hierarchy returns
SwiftUI identifiers, and what each call costs. If one Maestro process start costs several seconds,
the MCP session may be the only practical transport. Budgets below are placeholders until DEJA-18.

### 7.2 Action set (no model)

```text
actions(obs, goal, graph, ctx):
  for e in obs.elements where e.enabled and e.interactive and not e.overlay:
    selector = e.id ? {id} : e.label ? {text} : skip          # no point taps from search
    text-field or secure-field -> one input action per matching fixture value
    otherwise                  -> tap(selector)
  add driver.backActions(obs), scroll(down), scroll(up)
  drop config.destructiveDeny matches (delete, sign out, purchase, pay) unless the goal spec names them
  drop failed branches from this screen on this build
  score: +4 id selector, +3 untried here, +2 * goal-word overlap with id/label,
         +2 first step of a known path to the target, -3 leads back onto the current path
  return top K (default 12), ties broken by id, label, then position
```

Typed text comes only from config fixtures (Blankaday: `sam@example.com`, code `123456`) or a
generated token (`deja-1`). System UI is handled before the action set: a known alert gets its
configured answer, and an unknown one stops the step as `system-ui`.

### 7.3 Model questions are narrow and closed

Every model call is one narrow question with a closed answer set, the pattern WALK-016 already uses
for retrieval:

- The question offers IDs: action indices, or element IDs from the current snapshot.
- The answer may reference only offered IDs. An answer with an unknown ID is discarded, counted as
  one call, and replaced by the scripted choice.
- "Nothing warranted" (`{ none: true }`) is always a valid answer.
- The model never sees a raw hierarchy dump and never writes YAML.

```ts
export interface ActionChooser {
  readonly id: string           // 'scripted', 'openai-compatible:<model>', 'openclaw:<model>'
  pick(q: { goal: string; screen: ScreenSummary; actions: OfferedAction[] }):
    Promise<{ index: number } | { none: true }>
  isGoalScreen(q: { goal: string; screen: ScreenSummary }): Promise<boolean | 'unsure'>
  proposeAssertions(q: { goal: string; screen: ScreenSummary; offered: OfferedElement[] }):
    Promise<{ ids: string[]; rationale: string; confidence: number } | { none: true }>
}
```

### 7.4 The loop

```text
explore(goal, spec, budgets, chooser):
  key, build = adapter identity; G = pathStore?.graph(key) ?? empty
  ctx = spec.context ?? config.defaultContext        # must have explore: true
  target = spec.target                                # null -> draft mode (section 8)
  if target == null and chooser is scripted: return rejected('no target and no model')

  # 1. Known path first: no model, no new steps
  if target:
    plan = shortestPath(G, root(ctx), nodesSatisfying(G, target), context = ctx,
                        weight: verified-on-build 1, verified-elsewhere 3, candidate 6;
                        exclude failed, stale, nondeterministic, ambiguous)
    if plan:
      flow = stitch(ctx, plan, target)
      if verify(flow).passed: return save(flow, plan, provenance='observed')
      record failures; search from the deepest screen the run reached

  # 2. Frontier search
  reset(ctx); s = settle(); n = node(G, s)
  frontier = PriorityQueue(); push actions(s) from n
  while not stop():
    (from, action) = budgets.modelLeft > 0 and scoresClose(frontier)
                       ? chooser.pick(top K)          # provenance 'model-inferred'
                       : frontier.top()               # provenance 'observed'
    if current != from and not navigate(from): mark the failing step; continue
    if fingerprint(settle()) != from: mark 'stale'; continue
    r = act(segment(action)); after = settle()
    if r.crashed: crashes += 1; continue
    if not r.passed: record failedBranch(from, action, r.failure.category); continue
    to = node(G, after) or addNode(after)
    e = recordCandidate(ctx, from, action, to, snapshots)
    if (ctx, from, action) reached another screen before: mark e nondeterministic

    if target and satisfies(after, target):
      path = pathFromRoot(ctx, to); flow = stitch(ctx, path, target)
      run = verify(flow)
      if run.passed and run.observationsMatch(path) and negativeControl(flow, target):
        return save(flow, path)
      demote path; continue
    if not target and to is new and chooser.isGoalScreen(goal, to) == true:
      return draft(ctx, pathFromRoot(ctx, to), goal)  # section 8.2
    push actions(after) from to, depth + 1
  return rejected(reason, budgetsUsed, gaps)
```

`stitch` is the only writer of YAML: `appId`, one `launchApp` from the profile, state paths as
`runFlow` references, each segment followed by `extendedWaitUntil` on the next screen's marker,
then the approved target assertions unchanged.

### 7.5 Budgets and stopping rules

Budgets are hard limits per run. Nothing carries over between runs.

| Budget | Explore | Repair | Gap analysis |
| --- | --- | --- | --- |
| Model calls | 30 | 3 | 1 per candidate, 10 per run |
| Actions (`act`) | 60 | 5 | 0 |
| Resets | 10 | 2 | 0 |
| Depth from root | 20 | prefix + 1 | n/a |
| Wall time | 15 min | 3 min | 1 min |
| Full verifications | 3 | 1 | 0 |

These are placeholders until DEJA-18 measures step cost. When the model budget runs out, the run
continues with the scripted chooser. Stop early when the frontier is empty, a screen is revisited 5
times with no new transition, the app crashes twice, only denied actions are left, or the goal
fails WALK-016's adversarial-marker check. Every stop reports the reason, budgets used and model
calls counted honestly (or `null` with a basis).

### 7.6 Determinism and flakes

Ranking and tie-breaks are deterministic. A transition becomes verified only from the independent
full run, never from the step that found it. A screen and segment that reach two screens are
`nondeterministic`. A verification that fails then passes on a rerun is `flaky` (DEJA-7) and is
not saved.

### 7.7 With no model

Known-path composition, frontier search with the scripted chooser, replay, repair and gap analysis
all run with zero model calls. We expect scripted search to do well when IDs share words with the
goal (`settings.export` for "export journal") and badly otherwise. That is an expectation until DEJA-19
and DEJA-20 measure it. Draft mode needs a model.

### 7.8 Agent-facing results

Results that an agent reads (the explore report, MCP tools in DEJA-9) are structured JSON. They
never guess:

```json
{
  "status": "ambiguous",
  "ambiguous": true,
  "candidates": [{ "nodeId": "…", "marker": "screen.settings" }, { "nodeId": "…", "marker": "screen.settings" }],
  "truncated": false,
  "budgetsUsed": { "modelCalls": 4, "actions": 17, "resets": 2, "wallMs": 192000 }
}
```

`truncated: true` means a bound cut elements, candidates or steps. `ambiguous: true` comes with the
candidates instead of a pick.

## 8. AI testing: from a goal to a verified test

### 8.1 Where assertions come from

A saved test needs an approved target: the product assertions that say the goal happened.

| Source | Example | Approved by |
| --- | --- | --- |
| Owner goal spec | `goals/reveal.json`: `profile: uiTesting`, target `assertVisible id screen.reveal`, `assertVisible id reveal.results` | Writing the file |
| Owner YAML | The final assertions of a remembered flow | Writing the file |
| Natural-language goal | "Answer today's blank and see the results" | The owner's rules plus the threshold, or a person (8.2) |

The harness also adds **checkpoints**: `assertVisible` of each screen's marker along the path. They
show the route held. They are not product assertions and are labeled so in YAML comments.

### 8.2 A natural-language goal

1. Explore in draft mode until `isGoalScreen` says yes.
2. Ask `proposeAssertions` once. It is offered only element IDs (and, if config allows, exact
   labels) from the final screen's snapshot. The answer picks from them, with a rationale and a
   confidence. Unknown IDs are discarded. "Nothing warranted" ends the run with a draft and no
   proposal.
3. The harness writes those picks as `assertVisible` commands and runs the checks in 8.3.
4. The result is a `target-assertions` proposal in the review queue (section 9.3), with the draft
   YAML in `.deja/attempts/`. It is never saved to `WalkStore`.
5. The owner's assertion rules run first (config and goal spec: required assertion kinds, IDs or
   text that must be asserted, selectors never allowed as targets, per-goal minimums). A proposal
   that breaks a rule is refused whatever its confidence. Rules only tighten the bar.
6. If every rule and every check in 8.3 passes and the confidence is at or above the configured
   `targetAutoApproveThreshold`, the proposal is applied through `applyProposal` with trigger
   `auto-merged` and writes a `GoalTarget` with `source: 'auto-approved-proposal'`. Otherwise a
   person approves, edits or rejects it, which writes `source: 'approved-proposal'`. Either way the
   normal path then runs: stitch, verify from clean state, save, and the assertion hash freezes it.

Target auto-approval is off until the owner sets a threshold (decision 1, section 17).

### 8.3 Checks on every target assertion

- **It passes** in the independent verification run, from a clean root.
- **Negative control:** it fails on the start screen of the path. An assertion that is already true
  before the goal is done proves nothing, so the target is rejected.
- **Specific:** it names an ID or exact text. A bare `.*` pattern or an assertion on system UI is
  rejected.
- **Stable:** its ID is in the screen's fingerprint anchors, not an overlay or a dynamic ID.

### 8.4 Why the model can never weaken an assertion

- The model never writes YAML. It returns indices or offered IDs, and `stitch` writes every line.
- A target exists only once a person (or the owner's files) approved it. Its `assertionsSha256` is
  stored with the goal.
- Replay, repair, composition and deduplication must produce the same canonical assertion set. The
  engine compares hashes and refuses any flow whose hash differs. A changed target needs a new
  approval.
- Repair starts only from an action command (tap, input, scroll), never from an assertion. This is
  structural because an ID assertion failure is labeled `unknown`, not `product_assertion`
  (`failure-classification.ts:81-86`), and most of Blankaday's assertions are by ID.
- No option skips, loosens or deletes an assertion.

## 9. Repair, gaps and the review queue

### 9.1 Repair: re-find one broken step

```text
repairStep(flow, t, build):                          # t: the transition holding the failing action
  reset(t.contextId); navigate(t.fromNodeId)         # known prefix; stop if it fails
  if fingerprint(settle()) != t.fromNodeId: return rejected('screen changed before the step')
  for a in actions(settle(), goal)[0..5] ranked by similarity to the old selector:
    act(a); if node(settle()) == t.toNodeId: candidate = a; break
    navigate(t.fromNodeId)
  if none: return rejected
  newFlow = swap the action command in t's segment
  require assertionsSha256(newFlow) == assertionsSha256(flow)
  if not verify(newFlow).passed: return rejected
  propose({ kind: 'repair', flowDiff, confidence, rationale, evidence: [snapshots, runIds] })
```

The repair becomes a proposal. Applying it calls `persistVerifiedRepair` and writes a diff for the
owner's YAML file (or a draft PR through DEJA-6). Déjà never commits, pushes or merges.

### 9.2 Gap analysis: graph first, one narrow question each

Pure graph analysis picks candidates with no model cost:

| Candidate | Graph signal | Suggestion |
| --- | --- | --- |
| Bridge | A transition every goal in a context passes through (Today to You to Settings) | Extract a shared subflow; watch it closely |
| Isolated | A screen with no verified transition in or out, or a path that shares no screen | Explore from it, or drop it |
| Hub | A screen with many outgoing segments | Check its fingerprint isn't merging screens |
| Duplicate | Two segments from the same screen to the same screen | Keep the shorter one |
| Conflict | The same screen and segment reaching two screens | A `contradiction` proposal (informational) |
| Selector gap | A segment using a text or point selector, or a screen with no stable IDs | Add an accessibility ID (DEJA-6) |

Each candidate can then get one narrow model question, within the per-run budget, offering only the
IDs involved. "Nothing warranted" is valid. A static pass over Blankaday's flows would list
`"Discard changes?"`, `"Discard"`, `"A nickname for now. Add your name."`, `"Add your name"` and
the `point: "75%,70%"` tap in `reveal.yaml` as selector gaps.

### 9.3 The review queue

```ts
export type Proposal = {
  proposalId: string
  kind: 'target-assertions' | 'repair' | 'subflow-extraction' | 'dedup' | 'gap-id' | 'contradiction'
  confidence: number            // 0..1, from the graph signal and the model, stated with its basis
  rationale: string
  evidence: { snapshots: SnapshotRef[]; runIds: string[] }
  status: 'pending' | 'approved' | 'rejected' | 'auto-merged'
  createdAt: string
}
```

- `applyProposal(id, by)` is the only way a proposal changes state. Human approval and auto-merge
  take the same code path, and each adds a `GraphVersion` with trigger `approved` or `auto-merged`.
- Auto-merge is off by default. It is on only for kinds the owner lists, above a threshold the owner
  sets, and only for `repair`, `subflow-extraction` and `dedup`. Each still needs a verified pass.
  `target-assertions` follow their own rules and threshold (8.2). `gap-id` always needs a person.
- A `contradiction` is informational. Applying it changes nothing.
- `revert(version)` adds a `revert` version that restores an earlier state.

## 10. Optimization

### 10.1 Levers

| Lever | Saves |
| --- | --- |
| Known-path composition before any search | Model calls and steps. A goal on known screens needs zero |
| Subflow reuse (state paths such as sign-in) | Steps and resets |
| Model-free ranking; model asked only when top scores are close | Model calls |
| Hard per-run budgets | Bounds cost and time |
| Snapshot cache keyed by fingerprint | Repeated ranking work |
| Answer cache keyed by screen, offered action set, goal and model ID (current build only) | Repeated model calls in a rerun; still verified |
| Local repair of one step instead of re-exploring | Model calls and wall time |
| Shortest verified path among alternatives | Output length |
| Dedup proposals: drop repeated assertions on one screen, collapse waits, extract shared prefixes into subflows | Output length and duplication |

### 10.2 What the SDK measures

Every explore, repair and composition report carries a `metrics` section:

- model calls, and tokens in and out when the provider reports them (`null` otherwise), split by
  question kind (pick, goal screen, assertions, gap);
- `observe`, `act`, `reset` and verification counts, and wall time split into observe, act, model,
  verify and other;
- reuse: transitions and paths reused, cache hits, steps that came from memory versus new;
- output: steps, assertions, checkpoints and subflow references in the saved flow, before and after
  dedup;
- budgets used and the stop reason.

Claims about savings need like-for-like evidence: the same build, profile, goal and device; at
least 5 runs each way; medians and p90; raw reports kept. This follows the standing rule of no
speedup claim without raw evidence from equivalent full lifecycles.

## 11. Clean state, parameters and closure

- **Seeded data is the app's job.** Déjà uses only profiles from config or the owner's YAML and
  never invents launch arguments. Exploration runs only on profiles marked `explore: true`.
- **Preconditions are paths** listed in `config.stateChangingPaths`.
- **Parameters** use `${NAME}`. `RunnerRequest` (`contracts.ts:182`) gets an optional
  `env?: Record<string, string>`. Credential values never reach memory, reports or proposals.
  Literal values in the owner's YAML stay literal.
- **Closure.** A segment with `runFlow: file:` or `runScript` carries each file's relative path and
  SHA-256, and `segmentId` includes them. A changed subflow is a new segment. Escaping references
  are rejected (DEJA-2).

## 12. Safety rules

These keep TODO.md's standing rules.

1. Never save an unverified candidate. Only a full independent run from a clean root writes to
   `WalkStore` or marks a path verified.
2. Never weaken a product assertion (section 8.4).
3. Never auto-commit a repair. Proposals produce diffs or draft PRs. Auto-merge changes `.deja`
   only, never the owner's repository.
4. Fail closed on ambiguous, changed and unknown screens, bad schemas, tampered files and escaping
   paths. Ambiguity is reported with candidates, never resolved by a guess.
5. Records never answer for another build.
6. No secrets in memory, snapshots or prompts. Labels only when config allows.
7. Explore only safe profiles. Destructive actions are denied unless the goal spec names them.
8. Honest claims: a platform or saving is claimed only after its ticket links real evidence.

## 13. How it fits the SDK

All of this sits on `createDeja` (DEJA-5). `Deja.run` keeps its order: exact lookup first, lazy
factories after, so a warm hit builds no explorer, driver or chooser.

| Package | Adds |
| --- | --- |
| `@deja-qa/core` | Driver, chooser, path and proposal types; `PathStore`; pure planner and `stitch`; fingerprint v2; assertion-hash and negative-control checks |
| `@deja-qa/maestro` | `iosExplorationDriver`, `androidExplorationDriver`, flow sectioning, runner `env` |
| `@deja-qa/explorer-graph` (new) | `GraphExplorer implements WalkExplorer`, `scriptedChooser`, `openAiCompatibleChooser` |
| `@deja-qa/store-local`, `@deja-qa/store-sqlite` | `LocalPathStore`, `SQLitePathStore` |
| `@deja-qa/store-conformance` | `runPathStoreContractTests` |
| `@deja-qa/repair` | `GraphStepRepairer implements SelectorRepairer` |
| `@deja-qa/explorer-openclaw` | Unchanged, the existing Android option |

## 14. Options rejected

| Option | Why not |
| --- | --- |
| Key the graph by build | Every Blankaday iOS build is new, so memory would be empty every run |
| One YAML per goal plus semantic lookup | Goals still share nothing |
| Replace `WalkStore` with the graph | Breaks conformance and saved walks; the YAML must stay the deliverable |
| Per-platform explorer loops | Doubles the work and lets iOS and Android drift. Only observation, fingerprints and actions differ |
| Single-tap edges (WALK-017) | Can't hold scrolls, repeats, key presses or `runFlow env` |
| Model writes the YAML (today) | The model controls assertions, and every run needs a model |
| Model answers in free form | Can name elements that don't exist; closed answers are checkable |
| Text or screenshot fingerprints | Locale, theme and size change them; text can hold personal data |
| Fuzzy matches counted as matches | Turns uncertainty into false hits |
| Unrestricted auto-merge for assertions | Owner rules and checks must bound what the model may call a pass; it is allowed only above a threshold after them (8.2) |
| Random or reinforcement-learning exploration | Each step costs seconds on a simulator |
| Coordinate record and replay | Breaks across sizes (iPhone SE to the Duo outer display) |
| A graph database | Thousands of rows at most |

## 15. Risks

1. **Step-wise Maestro on iOS may be too slow or unreliable (biggest).** XCUITest snapshots are slow
   on large screens, and live state between separate `maestro test` calls is untested. DEJA-18 measures
   it first. Fallbacks: the MCP session transport, or fewer, larger steps.
2. **SwiftUI identifiers may be unstable** (merged children, identifiers passed down, lazy lists).
   Markers, filters and failing closed limit the damage; apps without IDs get gap reports.
3. **Hidden state** costs reuse (section 4.2).
4. **State leaks.** `clearState` keeps the iOS keychain and server data. Memory is as good as the
   app's test mode.
5. **Destructive actions.** The deny list and `explore: true` profiles reduce this, and can't
   guarantee it for an app with no test mode.
6. **System UI on iOS** can block a step in ways the harness doesn't know. Unknown alerts stop the
   step rather than being tapped.
7. **Records are thin while run history is parked**, so flake rates and trends aren't available.

## 16. Phased rollout

| Phase | Ticket | Model | Delivers |
| --- | --- | --- | --- |
| 1 | DEJA-18 explorer contract, iOS observation and fingerprints | No | Driver contract, iOS driver, fingerprint v2, measured step costs |
| 2 | DEJA-19 iOS cold exploration of one Blankaday flow | Optional | `GraphExplorer`, chooser, target checks, evidence on iOS 27.2 |
| 3 | DEJA-20 Android parity | Optional | Android driver; the same tests and explorer on the emulator |
| 4 | DEJA-21 path memory store | No | Sections, screens, transitions, provenance, versions, two-slot records |
| 5 | DEJA-22 reuse and optimization | No | Known-path composition, subflow reuse, caches, metrics, dedup proposals |
| 6 | DEJA-23 repair, gaps and review queue | Optional | Step repair, graph gap analysis, proposals, auto-merge threshold |

DEJA-19 and DEJA-20 keep their graph in memory for the run only. DEJA-21 makes it durable.

## 17. Decisions

Decided by the owner on 2026-11-22:

1. **Model-proposed target assertions** save on their own only when their confidence is at or above a
   configured threshold and they pass every target check (section 8). Below the threshold they end in
   a draft plus a proposal for a person. Owners can add rules in config and the goal spec: required
   assertion kinds, IDs or text that must be asserted, selectors never allowed as targets, and
   per-goal minimums. Rules are checked before the threshold, can only tighten the bar, and refuse a
   proposal whatever its confidence. The approved assertion hash still freezes a saved test.
2. **Auto-merge** is off by default. When on, only `repair`, `subflow-extraction` and `dedup` can
   apply on their own, above an owner-set threshold, inside `.deja` only. `gap-id` always needs a person.
3. **YAML parser:** the `yaml` npm package is approved, so parsing works on Node and Bun alike.
6. **Chooser transport:** any OpenAI-compatible chat API with JSON output. OpenClaw stays for Android.

Decided as defaults (the owner can override):

4. **Record slots while history is parked:** current build plus last passing build (DEJA-12 tracks
   full history).
5. **Screen markers:** optional `config.screenMarkerPattern`; Blankaday sets `^screen\.`.
7. **Snapshot labels:** off by default, because UI copy can hold personal data.
