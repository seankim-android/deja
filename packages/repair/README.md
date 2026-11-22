# Bounded checkout selector repair

This package implements only the validated Android Checkout repair from WALK-001/006. It is not a
general autonomous repair engine and makes no model calls. The patch is a deterministic, exact-text
edit to two files:

- `app/src/main/java/dev/walkcache/checkout/MainActivity.java`
- `app/src/main/res/values/ids.xml`

## Admission and write boundaries

A repair lifecycle starts with a previous successful, checksum-verified flow and independently runs
that unchanged flow against the current build. This diagnostic lookup is **not** a warm-cache hit.
Repair proceeds only when WALK-006 classifies the result as `selector`, marks it repair-eligible, and
the sole failing step is exactly:

```text
Tap on id: dev.walkcache.checkout:id/checkout_button
```

Product assertions, navigation, unknown/ambiguous failures, timeouts, runner/infrastructure errors,
other selectors, and unexpected passes are rejected. No plan, source write, rebuild, assertion edit,
or successful-memory write follows an ineligible result.

`repair.config.json` names workspace-relative canonical source roots and permits only `.java` and
`.xml`. Planning rejects any other affected file, lexical escapes, symlinked roots/components/files,
non-canonical files, unexpected content, and multiple replacement matches. Atomic source writes stay
in the existing source directories. A failed patch/build/install/verification restores the original
source bytes; successful source changes remain visible for human review and are never committed.

## Single attempt and shared deadline

A `BoundedCheckoutRepairer` instance can consume only one attempt. One overall monotonic deadline
covers diagnosis, patch planning/application, Gradle clean build, adb install of the rebuilt APK,
APK SHA-256 recomputation, full-journey verification, and atomic persistence. Build and Maestro child
processes receive only the remaining budget and terminate their own spawned process trees on timeout.
There is no retry loop. A timeout or failed verification cannot replace successful memory.

The rebuilt fingerprint must differ from the diagnosis fingerprint. The unchanged snapshotted flow,
including its final product assertion, then passes independently before
`LocalWalkStore.persistVerifiedRepair` atomically replaces metadata. Repair metadata retains earlier
validation history and previous exploration metrics; the exact prior flow and metadata bytes are also
saved under `.walkcache/patches/<attempt>/previous/`.

## Evidence and Git boundary

Every attempt preserves its diagnosis logs, proposed diff and plan when eligible, build/install logs
and report, verification logs/report when reached, previous memory/flow snapshots, and final repair
report. The CLI prints the exact diff and affected files before writing. It never invokes Git, commits
a repair, or creates a pull request. Generated `.walkcache/` data remains ignored; only reviewed,
secret-free reports and logs are copied into curated evidence.

From the Android spike, after seeding a known-good walk:

```sh
WALKCACHE_REPAIR_TIMEOUT_MS=240000 \
  bun scripts/walkcache.ts repair checkout --config repair.config.json
```

Focused deterministic checks:

```sh
bun test packages/repair/test --timeout=10000 --check
```

The real Android commands and linked successful/negative evidence are in the WALK-007 ticket.
