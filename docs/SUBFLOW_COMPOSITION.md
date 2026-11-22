# Verified login subflow composition (WALK-018)

WALK-018 demonstrates one deliberately narrow composition: reuse a verified Android login segment in
the materially different goal **“Sign in and check out Mochi sticker pack.”** It does not infer a
route, choose a subflow with a model, or add a second cache. `WalkCache.run`, exact goal identity,
lookup ordering, semantic retrieval, and repair orchestration are unchanged. Composition is an
explicit operation that always executes the complete expanded journey before storing it.

## Typed contract and exact identity

[`login-contract.json`](../spikes/001-android-walkcache/evidence/walk018/login-contract.json) is a
closed schema-1 `SubflowContract` validated by `packages/core/src/subflow.ts`:

- **Identity:** `dev.walkcache.checkout`, Android, `maestro/v1`, APK/build SHA-256
  `6aec5fea876938db14007ba3dbcb91f698832fe855886abd9dea1e7b28861759`.
- **Inputs:** required `username` (`text`) and `password` (`credential`). Input values are not legal
  contract-metadata fields. The YAML retains only the already accepted WALK-020 test-fixture values,
  not credentials for a real account.
- **Output:** `authenticatedCatalog`, a screen identified by `screen_catalog`.
- **Precondition:** exactly one clean app launch. The segment itself owns `clearState: true`.
- **Postcondition:** an `assertVisible` for `screen_catalog`, also required by the declared output.
- **Bounds and provenance:** 11 steps, 5 assertions, closed hashed source artifacts, and explicit
  `newDeviceRunRecorded: false`.

Unknown fields, raw input values, arbitrary screen text/UI dumps, path escapes, missing conditions,
duplicate names, and unsupported versions are rejected. `LocalVerifiedSubflowStore` stores the actual
YAML at `.walkcache/segments/login-v1/android-<sha256>.yaml` and its closed index at
`.walkcache/segments/index.json`. Lookup requires the same app/platform/schema/build plus optional
adapter/browser identity, verifies the current file checksum, and fails closed on invalid metadata.
It never performs semantic segment selection.

`MaestroLoginComposer.verifyAndSaveSegment` calls `WalkRunner` first. Only a passing run whose YAML
matches the app ID, clean launch, resource assertions, declared 11 steps and 5 assertions, and whose
runner also reports 5 assertions can reach `persistVerified`. Failed execution, mismatched contracts,
wrong identity, checksum changes, and missing YAML cannot become reusable success.

## Composition without copied login steps

The composed YAML contains one Maestro `runFlow` reference followed by a separately supplied checkout
continuation. It contains **no** launch, login-screen, input-control, login-button, username, or
password steps/values. The reference is relative from peer directories, so it resolves identically at
both required locations:

```text
candidate: .walkcache/composed/<goal>/candidate.yaml
persisted: .walkcache/flows/<goal>/<platform>-<composed-sha256>.yaml
reference: ../../segments/login-v1/android-<login-sha256>.yaml
```

The composer proves candidate and final directory references match **before** verification. Nested
subflows, a second launch, duplicated login controls, flow headers, and oversized/unbounded suffixes
are rejected. `countFlowAssertions` now follows bounded `runFlow` references and detects reference
cycles solely so reports count the complete expanded flow; existing flows without `runFlow` retain
the same assertion behavior.

Curated artifacts:

- [Independently reusable login YAML](../spikes/001-android-walkcache/evidence/walk018/segments/login-v1/android-c415a62dae8b1346213ac1f92c5201d2a8c17e01d98c84e6ab979f4d785f3266.yaml), SHA-256 `c415a62dae8b1346213ac1f92c5201d2a8c17e01d98c84e6ab979f4d785f3266`;
- [New checkout continuation](../spikes/001-android-walkcache/evidence/walk018/continuation/checkout.yaml); and
- [Composed YAML](../spikes/001-android-walkcache/evidence/walk018/composed/sign-in-and-check-out-mochi-sticker-pack/candidate.yaml), SHA-256 `59778cf6a4dd43d2cac304e2af0145fd6ffde695031e8154221832f5295908ab`.

| Reported separately | Reused login | New checkout continuation | Complete expanded journey |
| --- | ---: | ---: | ---: |
| Product steps (the reference directive itself is not counted) | 11 | 11 | 22 |
| Assertions | 5 | 9 | 14 |
| New composition/exploration model calls | 0 | 0 | 0 |
| Verification model calls | 0 | 0 | 0 |

The accepted original WALK-020 cold exploration had 13 model calls for the whole prior flow. Those
historical calls remain in its source report and are not misleadingly assigned to either split here.
WALK-018 used no explorer or model client for derivation, composition, verification, or exact replay.

## Clean-state proof and preservation

[`walk018-verification.json`](../spikes/001-android-walkcache/evidence/walk018/walk018-verification.json)
and its four raw deterministic runner logs record this independently runnable sequence in isolated
temporary storage:

1. Seed the accepted prior `checkout` goal memory and flow bytes.
2. Execute and independently save the login segment: one clean launch, 11 steps, 5 assertions,
   catalog postcondition passed.
3. Compose by reference and execute the complete candidate from clean state **before** goal
   persistence: 22 product steps, all 14 assertions, final `Order confirmed` and exact
   `Order #WC-1001 · Mochi sticker pack` assertions passed.
4. Save the composed walk, then invoke ordinary `WalkCache.run`. The stored `runFlow` reference still
   resolves; exact status is `hit`, outcome `passed`, model calls `0`, explorer constructions `0`,
   and all 14 assertions execute from clean state again.
5. Recompose the same goal with the unchanged YAML and an injected missing `checkout_button`. The
   deterministic runner reports a selector failure before confirmation. The composed walk is not
   persisted. Login index/YAML, the previous accepted goal flow, and complete goal memory—including
   the newly composed last-known-good validation history—remain byte-for-byte unchanged. The original
   `checkout` goal still resolves independently as an exact usable hit.

A separate integration test injects a filesystem metadata-replacement failure **after** a complete
passing verification. It proves the same login bytes, previous flow, and previous memory remain
unchanged and no new goal hit is manufactured. Segment-store tests likewise prove flow/index write
failures preserve an earlier verified segment byte-for-byte and orphan YAML is not loaded.

Evidence logs: [login](../spikes/001-android-walkcache/evidence/walk018/walk018-login-verification.log),
[clean composed run](../spikes/001-android-walkcache/evidence/walk018/walk018-clean-state-composition.log),
[stored exact replay](../spikes/001-android-walkcache/evidence/walk018/walk018-exact-replay.log), and
[failed recomposition](../spikes/001-android-walkcache/evidence/walk018/walk018-failed-composition.log).

## Exact provenance and reproduction

The flow split comes only from accepted **WALK-020 Android run `walk020-android-checkout-06`**:

- `cold-saved-checkout.yaml`, SHA-256
  `e95532ce04a4e125d3bd56e1b9d5d367b31dd67eed4441c56271ca52f3c3d0a1`;
- `cold-stored-memory.json`, independently passing at `2026-11-22T20:27:33.668Z`; and
- `cold-run-report.json`, outcome `cold-recorded`, 14 assertions for the same app/build/flow.

The deriver rehashes those artifacts, checks their identity and exact 11+11 sequence, rejects any
login duplication in composed YAML, and byte-compares all curated flow/contract outputs. The verifier
then runs real child processes through `MaestroWalkRunner` and `LocalWalkStore` using
`composition-maestro.cjs`, a bounded executable fixture for exactly this YAML subset. It launches no
emulator/simulator, contacts no service, and claims no new Android compatibility result.

```sh
bun install --frozen-lockfile
bun scripts/derive-walk018-composition.ts --check
bun scripts/verify-walk018-composition.ts --check
bun test packages/core/test/subflow.test.ts \
  packages/store-local/test/verified-subflow-store.test.ts \
  packages/maestro/test/subflow-composition.test.ts \
  packages/core/test/orchestration.test.ts --timeout=10000
```

The verification `--check` performs fresh process execution and byte-preservation checks in temporary
storage. Only wall-clock validation timestamps and hashes derived from those timestamps are excluded
from cross-run report comparison; flow hashes, raw logs, identity, counters, final assertions, exact
cache outcome, and every preservation decision must match curated evidence.

## Acceptance limits

This is a concrete login-plus-checkout composition, not general route planning, graph traversal,
subflow discovery, arbitrary parameter substitution, concurrent/multi-app composition, or proof that
Maestro on a device currently supports this app build. WALK-017 remains diagnostic-only and is not
used to authorize storage. There is no new real-device run, performance claim, new model exploration,
or generalized semantic correctness claim. Only the shown Android `maestro/v1` clean-launch login and
checkout suffix are demonstrated; Web/iOS composition and automatic recovery beyond existing cache
behavior are not claimed.
