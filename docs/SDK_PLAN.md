# Making Déjà a consumable SDK

Status: proposal. Nothing in this document is built yet unless it says "today".

## Goal

Today Déjà is a Bun monorepo with one product surface: the `deja` CLI in `apps/cli`.
Today each package and app is a Bun workspace with a private `@deja-qa/*` `package.json`, and they
import each other only by package name. `scripts/workspace-boundaries.test.ts` fails on a relative
cross-package import or an undeclared dependency. Nothing is published yet, and the `exports` maps
still point at `.ts` sources, so nobody outside this repo can install them.

The goal is to let two kinds of users install Déjà from npm:

1. **A team with Maestro tests in CI.** They want to replay verified flows with zero model calls,
   record pass/fail history, and catch stale builds. Most of them will use the CLI or the GitHub
   Action, not the library.
2. **An agent framework that wants cached UI walks.** It wants to call "run this goal" from code or
   over MCP, and only pay for a model on a cache miss.

### Usage example (proposed API)

```ts
import { createDeja } from '@deja-qa/core'
import { maestroRunner, androidAdapter } from '@deja-qa/maestro'
import { sqliteStore } from '@deja-qa/store-sqlite'
import { openClawExplorer } from '@deja-qa/explorer-openclaw'

const adapter = androidAdapter({ device: 'emulator-5554' })
const deja = createDeja({
  workspaceRoot: '.',
  app: { appId: 'com.example.shop', buildPath: 'app-debug.apk' },
  adapter,
  store: sqliteStore({ workspaceRoot: '.' }),
  runner: maestroRunner({ adapter }),
  explorer: () => openClawExplorer({ model: 'moonshotai-kimi-k3.1-pro-trial' }),
})

const result = await deja.run('checkout')   // RunResult, schema 1
```

`createDeja` is a thin wrapper over what the CLI does in `createOrchestration`
(`apps/cli/src/main.ts`). It would:

- build the `WalkIdentity` from the goal, `appId`, platform, `maestro/v1` and the adapter's build
  fingerprint, so callers do not compute SHA-256 hashes themselves;
- pass the explorer and repairer as lazy factories, which `Deja` already requires
  (`createExplorer`, `createRepairer?`), so a warm hit never builds a model client;
- default the `RunReporter` to a no-op, and offer the CLI's `JsonRunReporter` as an option.

`Deja` itself stays exported for callers who want full control.

### Seams that exist today

| Seam | Where | Shape |
| --- | --- | --- |
| Orchestrator | `packages/core/src/orchestration.ts` | `new Deja(deps)` with `run(identity)`, `explore(identity)`, `remember(identity, candidatePath)` |
| Dependencies | same file | `DejaDependencies`: `store`, `runner`, `reporter`, `createExplorer`, `createRepairer?`, `semantic?`, `now?`, `artifactId?` |
| Store | `packages/core/src/contracts.ts` | `WalkStore`: `lookup`, `lookupPreviousSuccessful`, `persistVerifiedCandidate`, `persistVerifiedRepair`, `recordValidation`, `list` |
| Runner | same | `WalkRunner.run({ flowPath, artifactDirectory, timeoutMs? })` returns `ExecutionResult` |
| Explorer | same | `WalkExplorer.explore({ identity })` returns `ExplorationResult` |
| Repair | same | `SelectorRepairer.repair({ identity })` returns `RepairResult` (`modelCalls: 0`, `repairCount: 0 \| 1`) |
| Reporter | same | `RunReporter.report(result)` |
| Semantic | same | `GoalEmbeddingProvider.embed(goal)`, opt-in, only after an exact `not-found` miss |
| Identity | same | `WalkIdentity`: `goal`, `appId`, `platform`, `flowSchemaVersion`, `buildFingerprint`, optional `adapter`, `browser`; goals go through `normalizeGoal` |
| Platform | `packages/maestro/src/platform-adapter.ts` | `MaestroExecutionAdapter`: `id`, `platform`, `device`, `buildFingerprint`, `cacheIdentity`, `commandArguments` |

## Package layout

All packages publish under the `@deja-qa` scope. See Risks for the name check.

| Package | Today's folder | Exports | Depends on |
| --- | --- | --- | --- |
| `@deja-qa/core` | `packages/core` | `Deja`, `createDeja` (new), all contracts, `normalizeGoal`, result schema, semantic and subflow types | nothing |
| `@deja-qa/maestro` | `packages/maestro` | `MaestroWalkRunner`, Android/Web/iOS adapters, `classifyMaestroFailure`, subflow composer | core |
| `@deja-qa/store-local` | `packages/store-local` | `LocalWalkStore`, `InvalidWalkMemoryError` | core |
| `@deja-qa/store-sqlite` | `packages/store-sqlite` | `SQLiteWalkStore` | core |
| `@deja-qa/store-conformance` | `packages/store-conformance` | `WalkStoreContractImplementation` and the shared suite | core |
| `@deja-qa/explorer-openclaw` | `packages/agent-openclaw` | `OpenClawWalkExplorer` | core, maestro |
| `@deja-qa/repair` | `packages/repair` | `BoundedSelectorRepairer`, `CheckoutRepairPolicy` | core, maestro, store-local (to remove) |
| `@deja-qa/semantic-local` | `packages/semantic-local` | `LocalLexicalEmbeddingProvider` | core |
| `@deja-qa/cli` (bin `deja`) | `apps/cli` | the `deja` CLI | all of the above |

Not published: `packages/metrics` (internal cold/warm analysis), `viewer/` and `src/` (the
optional local viewer), `spikes/`, `demo/`.

Dependency direction: `core` imports nothing from the repo. Plugins import `core` (and `maestro`
where they drive Maestro). Only the CLI imports everything. This matches the rule in
`docs/ARCHITECTURE.md` that core imports no Maestro, OpenClaw, filesystem or platform code.
Two edges break it today and need work before publishing:

- `repair` imports `store-local` in `packages/repair/src/index.ts`. It should take a `WalkStore`.
- `OpenClawWalkExplorer` takes a concrete `MaestroWalkRunner` and a `WalkStore`, because it
  verifies and persists the candidate itself. It should accept the `WalkRunner` interface.

## Plugin contracts

### Explorer (bring any model or agent)

Contract: `WalkExplorer.explore({ identity })` returns `ExplorationResult`. The explorer must:

- verify its candidate with an independent full-flow run before it reports `passed: true`;
- persist only through `WalkStore.persistVerifiedCandidate`;
- report `modelCalls` and `mcpCalls` honestly, or `null` with a `modelCallBasis` when not measured.

Default: `@deja-qa/explorer-openclaw`, Kimi K3.1 Pro on Cerebras through OpenClaw, Android only.
Today, Web and iOS use `UnsupportedPlatformExplorer` in `apps/cli`, which returns
`exploration-rejected` with zero model calls. That class should move to core so every host gets
the same refusal.

A second explorer (for example a plain MCP client loop on another model) is the real test that the
contract is general. Until one exists, call the explorer contract "provisional".

### Runner

Contract: `WalkRunner.run(request)` returns `ExecutionResult` with timing, assertion count,
artifacts, exit code and a conservative `WalkFailure`. `MaestroWalkRunner` is the only runner.
Failure classification lives in `@deja-qa/maestro` and stays advisory: `unknown` is never
repair-eligible.

### Store

Contract: `WalkStore`. The conformance suite in `packages/store-conformance` already runs the same
behavior tests against `LocalWalkStore` and `SQLiteWalkStore`, including injected write failures and
metadata tampering. Publish it as the contract test: a third-party store passes if it passes this
suite. It imports `bun:test` today, so it needs a runner-neutral entry (see Distribution).

### RepairPolicy

Contract today: `SelectorRepairer.repair({ identity })`. The only policy is `CheckoutRepairPolicy`:
one Checkout ID tap, two configured files, exact expected source, one attempt, one shared deadline.
It is a fixture policy, not a general one.

Proposed: split a `RepairPolicy` type (what is eligible, which files, which edit) from the
`BoundedSelectorRepairer` engine (diagnose, patch, rebuild, verify, restore). Keep the rules that
are not negotiable in the engine, not the policy: product assertion failures are never repaired,
`unknown` failures are refused, source bytes are restored on failure, and nothing is committed.

### Versioning

- Semver per package, released together from one changeset while below 1.0.
- `WALK_RESULT_SCHEMA_VERSION` (1), the memory schema (2) and `maestro/v1` are part of the public
  contract. A breaking change to any of them is a major version, with a reader for the old schema.
- `core` exports a `CONTRACT_VERSION`. Plugins declare the range they support in
  `peerDependencies` on `@deja-qa/core`.

## Distribution

- **Runtimes.** Node 20+ and Bun 1.4+. Today the code is Bun-first: `store-sqlite` and
  `store-conformance` use `bun:sqlite`, `store-conformance` uses `bun:test`, and `repair` calls
  `Bun.file`. Plan: replace `Bun.file` with `node:fs/promises`; give `store-sqlite` a driver seam
  (`bun:sqlite` on Bun, `node:sqlite` or `better-sqlite3` on Node; that dependency needs approval);
  split the conformance suite into runner-neutral cases plus thin Bun and Vitest/Node bindings.
- **Format.** ESM only, `"type": "module"`, `exports` maps, `.d.ts` files. Build with tsup.
  Relative cross-package imports become package imports through Bun workspaces.
- **CLI.** `npx @deja-qa/cli run checkout`. The config file is `deja.config.json`, runtime state lives
  in `.deja/`, and environment defaults use `DEJA_*` variables (for example `DEJA_MAESTRO`).
- **GitHub Action.** `deja-action`: install Maestro at a pinned version, run `deja run <goal>` for a
  list of goals, upload `.deja/reports/` as an artifact, and fail on exit codes 1, 3 and 4.
  Replay only by default; cold exploration in CI needs an explicit input and a provider key secret.
  It must run on a runner that has an emulator or simulator; the Action does not provide devices.
- **MCP server.** `@deja-qa/mcp` exposing `deja.run`, `deja.remember`, `deja.list` and `deja.inspect`.
  `deja.explore` stays off unless the host enables it, since it spends model calls.
  `@modelcontextprotocol/sdk` is already a root dependency.

## Phased roadmap

Sizes: S is under a day, M is a few days, L is a week or more.

### Phase 0: installable 0.1

| Item | Size | Done when |
| --- | --- | --- |
| Done: Bun workspaces and a `package.json` per package; replace relative `../../x/src` imports | M | `bun test` and `bun run build` pass with only package imports |
| Remove `repair` -> `store-local` edge; explorer takes `WalkRunner` | S | a test fails on a forbidden import |
| tsup build per package, ESM and `.d.ts` | S | `npm pack` of each package installs into an empty Node 20 project and type-checks |
| Remove `Bun.file`; sqlite driver seam | M | the store conformance suite passes on Node 20 and Bun 1.4 |
| Publish 0.1.0 under the chosen scope with provenance | S | `npm view @deja-qa/core version` prints 0.1.0 |

### Phase 1: stable API and CI

| Item | Size | Done when |
| --- | --- | --- |
| `createDeja` facade and `UnsupportedPlatformExplorer` in core | M | the usage example above runs against the Android fixture |
| API review, freeze contracts, add `CONTRACT_VERSION` | M | an API report (for example api-extractor) is checked in and diffed in CI |
| Runner-neutral store conformance package | M | a sample third-party in-memory store passes it under Vitest |
| `deja-action` | M | a sample repo replays the Android fixture in a workflow run with 0 model calls |
| `@deja-qa/mcp` server | M | an MCP client calls `deja.run` and gets a schema-1 `RunResult` |
| Docs site from `docs/` | S | published; every claim links to the code or evidence, like the README |

### Phase 2: wider coverage

| Item | Size | Done when |
| --- | --- | --- |
| iOS cold exploration | L | a real simulator run, with evidence, explores and verifies one goal |
| Web cold exploration | L | same, on the Chromium fixture |
| General repair policies (`RepairPolicy` split, more than one selector) | L | two policies pass the existing product-regression guard tests |
| Second explorer on another model or agent | M | it passes the same verification hooks as OpenClaw |
| Hosted cache (a remote `WalkStore`) | L | it passes the conformance suite, and multi-writer rules are written down |

## Risks and open questions

- **Name and scope.** Decided 2026-11-22: packages publish under `@deja-qa`, and the project
  domain is `deja-qa.com` (with `deja-qa.dev` if available). Plain `deja` is taken in too many
  places: the `deja` npm package (v0.1.12, a dotfiles CLI by another maintainer) and `deja.dev`.
  Checked 2026-11-22:
  - `@deja-qa/core` returns 404 on npm, and `deja-qa` and `dejaqa` are free as unscoped names.
  - `deja-qa.com`, `dejaqa.com`, `deja-qa.dev` and `dejaqa.dev` return 404 from registry RDAP (not
    registered). A registrar can still mark a name premium or reserved.
  - Still to do: create the `deja-qa` org at npmjs.com/org/create, and buy the domain.
  - Test data never uses the project domain. Fixture logins use the reserved `example.com`.
- **Maestro version pinning.** All evidence is on Maestro 2.12.0, and Maestro Web is beta. The
  runner should read `maestro --version` and refuse or warn outside a tested range. The Action pins
  the version.
- **Model-provider keys.** Today the provider key stays in OpenClaw's own credential config, and
  the generated config holds no secrets. An SDK must keep that rule: no key in `createDeja` options,
  reports or MCP responses. Replay needs no key at all, and the docs should say so first.
- **Bun-only code.** `bun:sqlite`, `bun:test` and `Bun.file` block Node users until Phase 0 is done.
- **Single writer.** Both stores assume one writer per workspace. Parallel CI shards that share a
  workspace can corrupt history. Document it, and make the hosted store solve it before release.
- **Honest claims.** Supported behavior is what the evidence shows: Android cold exploration and
  one Checkout repair on an API 34 emulator; Web and iOS replay of reviewed YAML only; no measured
  speedup. Package READMEs, the docs site and the npm description must not say more. A new
  platform claim needs linked run evidence first.
- **CLI name.** `deja` on npm belongs to someone else, so `npx deja` will not work. Document
  `npx @deja-qa/cli` and install the bin as `deja`.

## Next 5 steps

1. Claim the `@deja-qa` scope: create the `deja-qa` org at npmjs.com/org/create (the npm CLI cannot create orgs).
2. Done: Bun workspaces, one `package.json` per package, and package imports.
3. Remove the two forbidden edges (`repair` -> `store-local`, explorer -> concrete runner) and add a
   test that fails on forbidden imports.
4. Make the packages run on Node 20: drop `Bun.file`, add the sqlite driver seam, and run the store
   conformance suite on both runtimes in CI.
5. Build with tsup, `npm pack` each package into an empty Node project, then publish 0.1.0.
