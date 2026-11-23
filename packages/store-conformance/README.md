# WalkStore conformance suite

This package exports one behavior suite and runs it unchanged against:

- `LocalWalkStore`: JSON metadata plus content-addressed YAML; and
- `SQLiteWalkStore`: SQLite (`bun:sqlite` or `node:sqlite`) relational metadata plus content-addressed YAML.

The suites run on Bun with `bun:test` and on Node 22.13+ with `node:test`: each `run…ContractTests`
function takes an optional `ContractHarness` (`describe`, `test`, `expect`). With none, it uses `bun:test`.
On Node, pass `nodeHarness` from `@deja-qa/store-conformance/node`. `runPortableWalkStoreContractTests`
is the part of the contract that assumes nothing about where a store keeps its metadata, so an
in-memory or remote store can pass it; `test/node/memory-store.mjs` is one.

The adapter hooks only construct backends, close them, snapshot real metadata, inject write failures,
and corrupt real persisted state for negative tests.
The YAML, metadata, lookups, checksum validation, transactions, and orchestration under test are the
actual stores and production lifecycle—not in-memory type mocks.

## Shared behavioral coverage

The 23 cases exercised against each store cover:

1. first-run misses without creating metadata;
2. durable reopen, goal normalization, complete exact identity, and flow-schema partitioning;
3. stale builds, wrong app, and wrong platform;
4. Web adapter/browser identity;
5. missing YAML;
6. tampered YAML/checksum rejection;
7. malformed metadata;
8. unsupported schema;
9. invalid entries that must not be filtered into a hit;
10. duplicate exact identities;
11. lexical flow-path escape;
12. canonical symlink escape;
13. a missing metadata target behind an external directory symlink, with no outside creation;
14. metadata paths that traverse a non-directory on both reads and writes;
15. an existing valid external metadata directory swap, preserving bytes and preventing sidecars;
16. interruption before YAML rename and ignored temporary files;
17. failed candidate verification with byte-identical last-known-good YAML and metadata;
18. failed metadata replacement with byte-identical last-known-good state and ignored orphans;
19. append-only failed validation history and blocked reuse;
20. repair diagnostic lookup, validation/exploration continuity, and provenance;
21. changed repair-base rejection with preserved known-good state.

These are grouped into 23 `bun:test` cases because exact identity and repair assertions share one
case each. The shared tests perform no device operations and require no model/account/service.

## Actual warm orchestration

Each implementation also receives one separate `Deja.run` test using the production
`MaestroWalkRunner`, its existing deterministic local executable fixture, and production
`JsonRunReporter`. The runner really resolves persisted YAML, launches the child process, preserves
logs, classifies the result, and counts assertions. The test reads the real JSON report and output
artifact, asserts the subprocess exit code is zero, confirms validation history was persisted, and
counts explorer/model and repairer constructions as zero. This is no-device orchestration evidence,
not a claim of real platform execution.

## Run

```sh
bun test packages/store-conformance/test/conformance.test.ts \
  packages/store-local/test/local-store.test.ts \
  packages/store-sqlite/test/sqlite-store.test.ts --timeout=10000 --check
```

`LocalWalkStore` additionally retains its historical JSON schema-1 upgrade test.
`SQLiteWalkStore` additionally aborts a real transaction with a SQLite trigger to prove rollback.
Backend README files describe setup, recovery, durability, and concurrency limits.
