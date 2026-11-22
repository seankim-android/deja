---
id: WALK-019
title: Prove store independence with a second implementation
milestone: P1 plugin conformance
priority: P1
status: done
depends_on: [WALK-008]
---

# WALK-019 — Prove store independence with a second implementation

## Outcome

Validate the `WalkStore` boundary using the same behavior tests against two implementations.

## Checklist

- [x] Extract shared contract tests for identity, retrieval, persistence, corruption, history, and
  replacement failures.
- [x] Run them unchanged against local filesystem storage and a second small implementation.
- [x] Preserve zero-model cache hits and atomic last-known-good behavior in both implementations.
- [x] Keep the second store local/simple; no hosted account system or remote service is required.
- [x] Document durability, concurrency, setup, and limitations honestly.

## Implementation

[`packages/store-conformance/src/walk-store-contract.ts`](../packages/store-conformance/src/walk-store-contract.ts)
exports one 20-case behavioral suite. [`conformance.test.ts`](../packages/store-conformance/test/conformance.test.ts)
invokes that exact function twice with thin real-backend adapters:

- existing `LocalWalkStore` (JSON metadata + canonical content-addressed YAML);
- new `SQLiteWalkStore` in [`packages/store-sqlite`](../packages/store-sqlite/README.md), using Bun's
  built-in SQLite driver for real relational walk/validation/repair metadata and the same actual YAML
  file persistence.

The adapters only construct/close stores, snapshot persisted metadata, inject deterministic write
failure, and tamper with real state for corruption cases. They do not implement an in-memory store,
replace checksum/schema behavior, or fake persistence. The suite verifies first-run misses, reopen
durability, normalized/exact goal/app/platform/schema/build/adapter/browser identity, all named miss
reasons, missing/tampered YAML, malformed/unsupported/invalid/duplicate metadata, lexical and
canonical path escapes, interrupted flow rename, rejected verification, failed metadata replacement,
append-only validation history, diagnostic repair lookup, repair provenance/history, changed repair
bases, and last-known-good YAML/metadata snapshots.

SQLite metadata replacement is one real `BEGIN IMMEDIATE` transaction. The backend-specific
[`sqlite-store.test.ts`](../packages/store-sqlite/test/sqlite-store.test.ts) installs a SQLite trigger
that aborts after transaction work begins; rollback restores the prior walk rather than committing a
partial delete/insert. Filesystem-specific schema-1 upgrade remains covered separately in
[`local-store.test.ts`](../packages/store-local/test/local-store.test.ts). Existing CLI wiring still
uses `LocalWalkStore`; no hosted account/service, remote infrastructure, migration, or CLI store
selector was added.

## Acceptance evidence

### Same suite plus one actual orchestration per store

From the repository root:

```sh
bun test packages/store-conformance/test/conformance.test.ts \
  packages/store-local/test/local-store.test.ts \
  packages/store-sqlite/test/sqlite-store.test.ts --timeout=10000 --check \
  2>&1 | tee spikes/001-android-walkcache/evidence/walk019-store-conformance.txt
```

[`walk019-store-conformance.txt`](../spikes/001-android-walkcache/evidence/walk019-store-conformance.txt)
records **44 tests / 153 assertions / 0 failures** across three files:

- the identical 20 shared cases pass against each store (40 tests);
- the local legacy-schema and real SQLite rollback cases pass (2 tests); and
- one warm `WalkCache.run` passes per store (2 tests).

Each orchestration uses production `MaestroWalkRunner`, which resolves stored YAML and launches its
existing deterministic local child-process executable fixture, plus production `JsonRunReporter`.
The test reads actual output and latest-report artifacts, checks child exit code 0 and one real parsed
`assertVisible` count, and confirms a second passing history entry persisted. Both output records say
`cacheStatus: "hit"`, `outcome: "passed"`, `modelCalls: 0`, `explorerConstructions: 0`,
`repairerConstructions: 0`, and `childProcessExitCode: 0`. `WalkRunner` is not mocked. This is local
runner/store orchestration evidence, **not** a real Android/Web/iOS device execution claim.

### Full portable regression and static gates

```sh
bun test --timeout=10000 --check \
  2>&1 | tee spikes/001-android-walkcache/evidence/walk019-all-tests.txt
bun run lint 2>&1 | tee spikes/001-android-walkcache/evidence/walk019-lint.txt
bun run build 2>&1 | tee spikes/001-android-walkcache/evidence/walk019-build.txt
```

[`walk019-all-tests.txt`](../spikes/001-android-walkcache/evidence/walk019-all-tests.txt) passes
**132 tests / 719 assertions / 0 failures across 18 files**.
[`walk019-lint.txt`](../spikes/001-android-walkcache/evidence/walk019-lint.txt) reports 0 warnings and
0 errors, and [`walk019-build.txt`](../spikes/001-android-walkcache/evidence/walk019-build.txt) passes
`tsc -b && vite build`. No emulator, viewer server, model credential, hosted account, or remote
service is needed. No emulator was started or changed by this ticket.

## Durability, atomicity, concurrency, and unresolved limitations

[`packages/store-sqlite/README.md`](../packages/store-sqlite/README.md) documents setup, schema,
`close()`, local files, `synchronous = FULL`, foreign keys, rollback journals, YAML checksum/path
rules, and single-writer usage. The top-level [architecture](../docs/ARCHITECTURE.md) distinguishes
JSON atomic rename from SQLite transactions.

The honest boundary is:

- Successful YAML rename precedes metadata commit. An interrupted metadata replacement leaves the
  previously referenced YAML and logical metadata intact; an orphan new YAML file is ignored.
- SQLite makes each relational metadata replacement all-or-nothing. It does **not** make YAML rename
  and database commit one cross-resource transaction.
- Deterministic rename/commit failures and real SQL rollback are tested. Power loss, disk failure,
  `fsync` completion, and every possible process-crash point are not certified.
- SQLite serializes immediate writers with a 1-second busy timeout, but high-level operations read a
  snapshot before the write transaction and do not version/merge competing updates. Only one
  persistence/validation writer may operate per workspace; there is no distributed or cross-machine
  coordination.
- SQLite accepts only schema 2 and does not import legacy JSON. The production CLI store selector is
  not in scope. Semantic retrieval remains WALK-016 and is not added here.

These limitations are explicit operational boundaries, not failing acceptance behavior. Both real
stores and both zero-model warm orchestrations pass the required evidence.
