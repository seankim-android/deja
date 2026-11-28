# Releasing the standalone `deja` binary

Each release attaches standalone native Rust `deja` binaries to a GitHub release. A consumer needs
no Bun, Node, Java, or external Maestro runtime.

**A release is cut only when Sean asks.** Do not push a `v*` tag, run the release workflow, or
create a GitHub release on your own. Everything below describes the steps for when he does.

## What a release contains

| File | Platform |
| --- | --- |
| `deja-<version>-darwin-arm64` | macOS on Apple silicon (built and smoke-checked on `macos-14`) |
| `deja-<version>-linux-x64` | Linux x86-64, `glibc >= 2.35` baseline (built and smoke-checked on `ubuntu-22.04`; static `musl` evaluation tracked in `DEJA-70`) |
| `SHA256SUMS` | SHA-256 checksum of each binary above |

## Cut a release (only when Sean asks)

1. Set `[workspace.package].version` in root `Cargo.toml` and merge it to `main` through a pull request.
2. Tag the merge commit. The tag must be `v` plus that version, and the tagged commit must be on
   `main`, or the workflow stops.

   ```sh
   git fetch origin && git tag v0.1.0 origin/main && git push origin v0.1.0
   ```

3. The [Release workflow](../.github/workflows/release.yml) runs on the tag push (or on manual
   dispatch with the tag name). A `resolve` job pins the tag to one commit on `main` and verifies
   `Cargo.toml`, both platform builds check out that exact SHA, run
   `deja-repo-check --require-clean --check-chart`, and build the native binaries. Regular releases
   stay explicitly fail-closed until `DEJA-65` and `DEJA-70` replace the release gate with an
   end-to-end deterministic clean-PATH behavioral smoke test (`remember` + warm `run` + `gate` +
   `deja-summary v=1`), after which the workflow re-verifies the tag did not move, computes
   `SHA256SUMS`, and creates a **draft** GitHub release.
4. Open the draft, verify the files and `SHA256SUMS`, then publish it by hand.

The workflow runs only on a pushed `v*` tag or manual dispatch, never on pull requests or pushes to
`main`. Only the `release` job has `contents: write`.

## Build and check locally

```sh
cargo build --locked --release --bin deja
env -i PATH=/usr/bin:/bin HOME="$HOME" target/release/deja --version
env -i PATH=/usr/bin:/bin HOME="$HOME" target/release/deja --help
env -i PATH=/usr/bin:/bin HOME="$HOME" target/release/deja guide
cargo run --locked --offline --bin deja-repo-check -- --require-clean --check-chart
```

## Pin a version and checksum (consumers)

Record the version and SHA-256 of your platform's binary in your repository from `SHA256SUMS` on
the release page:

```sh
DEJA_VERSION=0.1.0
DEJA_SHA256=<sha256 of deja-0.1.0-darwin-arm64>
```

[`docs/examples/install-deja.sh`](examples/install-deja.sh) downloads the pinned binary, verifies
the checksum before making it executable, caches it, and prints its path.
