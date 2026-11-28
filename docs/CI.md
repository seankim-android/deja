# CI checks and local platform gates

## CI: Pure-Rust portable checks (Ubuntu & macOS, no devices, no model)

[`.github/workflows/rust-checks.yml`](../.github/workflows/rust-checks.yml) runs on `main` pushes,
pull requests, and manual dispatch across `ubuntu-latest` and `macos-latest` with read-only
repository permissions. Each job checks out the exact commit, installs the stable Rust toolchain
(`rustfmt`, `clippy`), and runs:

```sh
cargo fmt --all -- --check
cargo fetch --locked
cargo clippy --locked --offline --workspace --all-targets --all-features -- -D warnings
cargo test --locked --offline --workspace --all-features
DEJA_BOUNDARIES_ALL_TARGETS=1 cargo test --locked --offline -p deja-cli --test workspace_boundaries --all-features
cargo run --locked --offline --bin deja-repo-check -- --require-clean --check-chart
```

`deja-repo-check` validates every strict tracked JSON file, rejects generated cache/build/dependency/binary
artifacts (including tracked `target/` files), checks every local Markdown link, scans text for
private keys/token shapes and machine-specific home paths, verifies `docs/results-chart.svg` matches
`demo/live-evidence/` run reports, and fails if the checkout changed during checks.

## Reproduce a clean checkout

```sh
git clone https://github.com/deja-run/deja.git
cd deja
cargo fmt --all -- --check
cargo fetch --locked
cargo clippy --locked --offline --workspace --all-targets --all-features -- -D warnings
cargo test --locked --offline --workspace --all-features
DEJA_BOUNDARIES_ALL_TARGETS=1 cargo test --locked --offline -p deja-cli --test workspace_boundaries --all-features
cargo run --locked --offline --bin deja-repo-check -- --require-clean --check-chart
```

No ignored cache, emulator state, credentials, or model keys are required for these checks.

## Platform integration is separately verified

CI does **not** relabel unit tests as real iOS/Android/Web device runs. Live device proofs live
alongside their fixtures and spike evidence:

- Native iOS Simulator (`DEJA-60`): [`SPIKE_EVIDENCE.md`](../drivers/ios/DejaXCTestRunner/SPIKE_EVIDENCE.md)
  (and historical Maestro 2.12 baseline [`walk011-warm.txt`](../spikes/003-ios-deja/evidence/walk011-warm.txt)
  / [`walk011-remember.txt`](../spikes/003-ios-deja/evidence/walk011-remember.txt)).
- Android baseline: [`walk010-android-warm.txt`](../spikes/001-android-deja/evidence/walk010-android-warm.txt),
  [`walk012-readme-review.txt`](../spikes/001-android-deja/evidence/walk012-readme-review.txt), and
  [`SUBFLOW_COMPOSITION.md`](SUBFLOW_COMPOSITION.md).
- Web baseline: [`walk010-warm.txt`](../spikes/002-web-deja/evidence/walk010-warm.txt),
  [`walk010-remember.txt`](../spikes/002-web-deja/evidence/walk010-remember.txt),
  [`walk010-product-regression.txt`](../spikes/002-web-deja/evidence/walk010-product-regression.txt),
  and [`spikes/002-web-deja/README.md`](../spikes/002-web-deja/README.md).
