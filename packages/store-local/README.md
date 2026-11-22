# Conservative local walk store

`LocalWalkStore` persists only independently verified Maestro flows. Unit tests use isolated OS
temporary directories and fake filesystem failures; they do not need an emulator, Maestro, OpenClaw,
or a model.

## Identity and reusable memory

A reusable hit requires all of the following to match exactly:

- normalized goal (`trim`, lowercase, collapse whitespace);
- app ID and platform;
- flow schema (`maestro/v1`);
- APK/build SHA-256; and
- the stored flow SHA-256, recomputed from the canonical flow file before every hit.

Stored flow paths must remain under `.walkcache/flows` after symlink resolution. Temporary files,
unreferenced flows, stale builds, wrong app/platform/schema identities, missing flows, and walks whose
latest validation failed are never hits. A checksum mismatch, malformed JSON, unsupported schema,
invalid entry, duplicate exact identity, or path escape raises `InvalidWalkMemoryError`; entries are
never silently filtered to manufacture a hit.

Current memory uses schema 2. Valid WALK-003 schema-1 metadata is read conservatively when it already
has a flow checksum: its flow schema is the known `maestro/v1`, and its existing `lastValidation`
becomes the one history entry. The next validation/persistence write emits schema 2. A failed latest
validation remains failed during this upgrade. Other schemas and incomplete legacy entries are
invalid.

## Atomic replacement and validation history

Verified candidates are written to a content-addressed path:

```text
.walkcache/flows/<normalized-goal-slug>/<platform>-<flow-sha256>.yaml
```

The flow is renamed before the metadata, so a failed metadata replacement leaves the previous
known-good flow and metadata byte for byte unchanged. A newly written flow without a metadata
reference is ignored. Likewise, a `*.tmp` left by interruption before rename is never loaded.
Replacement writes retain only the current goal/app/platform entry, as the WALK-003 lifecycle did.

Each persisted walk has `lastValidation` and append-only `validationHistory` entries containing
`passed`, `at`, and `flowSha256`. A failed replay is appended as failed and makes later lookup return
`failed-validation`; it never overwrites history with a success or remains reusable merely because an
older validation passed. The flow bytes are left intact for diagnosis.

## Explicit repair provenance (WALK-007)

`lookupPreviousSuccessful` is a separate diagnostic API used only by the bounded repair lifecycle. It
can select an earlier successful build's checksum-verified flow, but it is never a warm-cache hit and
does not authorize a write by itself. The caller must independently execute that flow and satisfy the
strict selector classification before planning a patch.

`persistVerifiedRepair` accepts a passing verification only while the named previous successful
build/flow still exists unchanged in valid memory. Its atomic replacement has `origin: "repair"`,
appends to the previous validation history, preserves previous exploration metrics, and adds
`repairHistory` containing the attempt ID, proposed patch path, and previous build/flow SHA-256. The
exact prior metadata/flow bytes are also preserved in the repair attempt artifacts. A failed build or
verification never reaches this method.

## Invalid-memory recovery and exploration

Corruption is not a cache hit and is not silently repaired into a pass:

1. `run` reports an identity/missing-flow/failed-validation miss with `recovery: "explore-required"`.
   Malformed memory or an unsafe/checksum-mismatched flow reports `InvalidWalkMemoryError` and exits
   nonzero without launching Maestro.
2. Preserve the ignored artifacts for diagnosis. If metadata itself is invalid, quarantine the ignored
   `.walkcache/` directory rather than editing a flow or deleting individual entries to force a hit:

   ```sh
   cd spikes/001-android-walkcache
   mkdir -p .tmp
   mv .walkcache ".tmp/invalid-walkcache-$(date +%s)"
   ```

3. Run the WALK-003 cold lifecycle to explore and independently verify a replacement:

   ```sh
   . ./scripts/env.sh
   WALKCACHE_OPENCLAW="$(command -v openclaw)" \
     bun scripts/walkcache.ts explore checkout --config explore.config.json
   ```

The store deliberately rejects invalid existing metadata during persistence instead of discarding it.
Only a complete passing cold exploration after quarantine can create new successful memory. This is a
recovery procedure, not a claim that exploration or a warm replay passed.

## Focused deterministic verification

From the repository root:

```sh
bun test packages/store-local/test/local-store.test.ts --timeout=10000 --check
```

The test names enumerate first-run/hit/identity cases, stale builds, missing flows, corruption,
canonical path escapes, interruption before rename, failed verification/replacement preservation,
validation history, and WALK-003 schema upgrade. `renameFile` is injectable solely to deterministically
model filesystem failures; the production default is `node:fs/promises.rename`.
