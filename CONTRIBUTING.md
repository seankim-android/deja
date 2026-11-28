# Contributing to Déjà

**Pull requests from outside contributors are not accepted right now.** Please open a [GitHub issue](https://github.com/deja-run/deja/issues) for bugs and ideas. The rest of this page applies once outside pull requests open.

Thanks for helping. This page covers setup, the checks CI runs, what a good
pull request looks like, and the Contributor License Agreement (CLA).

## Setup

You need [Rust](https://www.rust-lang.org/tools/install) `1.89` or newer with `rustfmt` and `clippy`.

```sh
git clone https://github.com/deja-run/deja.git
cd deja
cargo fetch --locked
```

The checks below need no device, emulator, or model credentials. Running the live iOS Simulator spike requires macOS and Xcode; see the Quick start in [README.md](README.md) and [docs/REFERENCE.md](docs/REFERENCE.md).

## Checks

These are the steps `.github/workflows/rust-checks.yml` runs on Ubuntu and macOS. Run them from the repository root before you open a pull request:

```sh
cargo fmt --all -- --check
cargo fetch --locked
cargo clippy --locked --offline --workspace --all-targets --all-features -- -D warnings
cargo test --locked --offline --workspace --all-features
DEJA_BOUNDARIES_ALL_TARGETS=1 cargo test --locked --offline -p deja-cli --test workspace_boundaries --all-features
cargo run --locked --offline --bin deja-repo-check -- --require-clean --check-chart
```

`deja-repo-check --require-clean --check-chart` fails if the checkout has any untracked or modified
files, broken local Markdown links, secret/path leaks, or an out-of-date `docs/results-chart.svg`,
so commit (or remove) everything first.

## Pull requests

- Keep each pull request small and focused on one change.
- Add or update tests for behavior changes, and say in the description how you
  checked the change (commands run and their results).
- Never commit secrets, tokens, API keys, or `.env` files. Use `example.com`
  for any email addresses in fixtures.
- Use [GitHub issues](https://github.com/deja-run/deja/issues) to report
  bugs or propose changes.

## Contributor License Agreement

Before your first pull request can be merged, you need to accept the
[Déjà Individual Contributor License Agreement](CLA.md).

**Why:** Déjà is free under Apache-2.0, and the maintainer may later offer
paid features under other terms. The CLA lets the maintainer license
contributions under any terms, including commercial ones. You keep the
copyright in your work; the CLA is a license, not a transfer of ownership.

**How:** once CLA checking is enabled on this repository, the CLA Assistant
bot will comment on your pull request with a link; sign in with GitHub and
accept the agreement. You only need to do this once per CLA version. The CLA
is currently a draft pending legal review. It is an individual agreement: if
you are contributing on behalf of an employer, open a GitHub issue first to
arrange a corporate agreement.

## License

Déjà is licensed under the [Apache License 2.0](LICENSE). You license your
contributions to the maintainer under the [CLA](CLA.md). The maintainer
distributes Déjà to users under Apache-2.0 and may also offer other terms.
