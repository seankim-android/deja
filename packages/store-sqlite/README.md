# SQLite walk store

`SQLiteWalkStore` is a second, entirely local implementation of the vendor-neutral `WalkStore`
contract. It uses Bun's built-in `bun:sqlite` driver for real relational metadata and ordinary files
for independently runnable Maestro YAML. It has no hosted service, account, native npm dependency,
or OpenClaw/model requirement.

## Setup

Use Bun 1.4 or later from the repository root:

```sh
bun install --frozen-lockfile
```

Inject the store through the existing `WalkStore` boundary:

```ts
import { SQLiteWalkStore } from './packages/store-sqlite/src/index.ts'

const store = new SQLiteWalkStore({ workspaceRoot: '/path/to/local/workspace' })
// Pass store to Deja, explorer, or repair wiring normally.
store.close()
```

Call `close()` when the owning lifecycle ends. The production CLI still defaults to
`LocalWalkStore`; WALK-019 proves the plugin boundary but does not add a CLI store-selection option,
convert existing JSON memory, or claim that arbitrary external stores are supported.

## On-disk representation

```text
.deja/
  memory/walks.sqlite
  flows/<normalized-goal-slug>/<platform>-<flow-sha256>.yaml
```

The SQLite database has `PRAGMA user_version = 2` and three tables:

- `walks`: exact goal/app/platform/schema/build/adapter/browser identity, YAML path/checksum,
  latest validation, origin, and optional exploration metrics;
- `validation_history`: ordered, append-only validation entries keyed to a walk; and
- `repair_history`: ordered repair provenance keyed to a walk.

YAML remains a canonical regular file under `.deja/flows`; every hit resolves the path,
enforces the workspace/flow-directory boundary, reads the bytes, and recomputes SHA-256. Missing
YAML is a miss, while changed YAML, malformed metadata, unsupported schema versions, invalid or
duplicate identities, unsafe paths, and inconsistent history fail closed. Database schema 2 is the
only accepted SQLite schema. There is no JSON-schema-1-to-SQLite importer.

Every metadata read and write separately resolves a safe canonical `walks.sqlite` target before using
a cached connection or opening a database. If any path component does not exist yet, resolution stops
at the nearest existing ancestor within the workspace, canonicalizes only that ancestor, and projects
the remaining path without walking above the workspace. Existing targets must be regular files and
existing ancestors for pending paths must be directories. A symlink or path escape outside the
workspace, a dangling/unresolvable link, or non-directory traversal raises `InvalidWalkMemoryError`.
Consequently an external database cannot be read, created, replaced, or deleted through
`.deja/memory`; the shared regression also proves an already valid external database remains
byte-identical with no new journal/sidecar files after rejected lookup and persistence.

## Persistence, durability, and replacement

A verified candidate follows these steps:

1. Validate the candidate and all existing metadata, then read and hash the actual candidate YAML.
2. Write a uniquely named `.tmp` YAML file and atomically rename it to its content-addressed final
   path. Unreferenced final files and temporary files are never loaded as cache hits.
3. Replace all affected relational metadata in one `BEGIN IMMEDIATE` SQLite transaction. Deleting
   previous rows and inserting walks, validation history, and repair history either all commit or
   all roll back. A mid-transaction SQL failure retains the prior metadata; the conformance tests
   exercise this with an actual SQLite trigger, not just an injected exception.

`PRAGMA synchronous = FULL` requests SQLite's full durability behavior, and the rollback journal uses
`journal_mode = DELETE`. `PRAGMA foreign_keys = ON` protects relational ownership. Reopening a new
store instance reads the persisted database and YAML, which the shared suite verifies.

The YAML rename and SQLite commit are deliberately ordered so a failure before metadata commit leaves
last-known-good metadata and its referenced YAML unchanged. A newly renamed but unreferenced YAML
file is an ignored orphan. Record-validation writes use the same metadata transaction mechanism and
leave YAML bytes intact.

**Boundary:** filesystem rename plus database commit is a two-file/local-database protocol, not one
atomic transaction spanning both resources. The tests establish deterministic interruption and SQL
rollback behavior; they do not certify power loss, OS/drive failure, `fsync` completion, or every
possible process-crash point. `atomicWrite: true` in `PersistedWalk` retains the existing contract
meaning of atomic successful replacement, not distributed atomicity.

## Concurrency and scope limits

SQLite serializes `BEGIN IMMEDIATE` writers and `busy_timeout` is 1,000 ms. Separate readers may use
their own connections. However, each high-level operation reads a metadata snapshot before entering
its later write transaction, and replacement writes the resulting snapshot as a unit. There is no
cross-operation optimistic version check, application lock, multi-writer merge, or distributed
coordination. Run only **one persistence/validation writer at a time per workspace**; concurrent
candidates can overwrite another writer's logical update, and repair still requires its previous
base to match at the initial check. A busy database can fail rather than wait indefinitely.

The store is for one local workspace and local filesystem semantics. It is not a network database,
cross-machine lock, remote cache, backup system, or semantic retrieval implementation. Existing
repair/explorer policy remains outside storage and unchanged. Canonical checks are repeated per
operation, but do not claim protection against a hostile process racing symlink/path-component
replacement between the check and the subsequent filesystem open.

## Verification

From the repository root:

```sh
bun test packages/store-conformance/test/conformance.test.ts \
  packages/store-local/test/local-store.test.ts \
  packages/store-sqlite/test/sqlite-store.test.ts --timeout=10000 --check
```

The identical exported suite runs 20 tests against each store, plus one actual `Deja` warm
orchestration per store through production `MaestroWalkRunner` and `JsonRunReporter`. The runner
executes its existing deterministic local child-process fixture, not a mocked `WalkRunner`; this
proves runner/store orchestration and zero model construction, not an Android/Web/iOS device result.
Backend-specific tests retain JSON schema-1 upgrade and prove real SQLite rollback. See the WALK-019
ticket for curated passing output and exact counts.
