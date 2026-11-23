# Releasing the standalone `deja` binary

Each release attaches a standalone `deja` to a GitHub release: a single file with the Bun
runtime built in. A consumer needs no Bun and no `node_modules`.

**A release is cut only when Sean asks.** Do not push a `v*` tag, run the release workflow,
create a GitHub release or run `npm publish` on your own. Everything below describes the steps for
when he does.

## What a release contains

| File | Platform |
| --- | --- |
| `deja-<version>-darwin-arm64` | macOS on Apple silicon (smoke-checked in CI) |
| `deja-<version>-linux-x64` | Linux on x86-64 (built, not run in CI) |
| `SHA256SUMS` | checksum of each binary above |

A build from a tree with uncommitted changes reports `<sha>-dirty` as its commit.
`deja --version` prints `deja <version> (commit <sha>, contract <CONTRACT_VERSION>)`. A run from
source prints `commit dev`.

The binary runs `run`, `list`, `inspect`, `remember`, `doctor` and `engine install`. One limit:
`deja explore` with the real OpenClaw explorer starts its MCP proxy script with `process.execPath`,
which in a compiled binary is `deja` itself. Cold exploration therefore still needs a
Bun-based setup. The binary refuses it at once with "explore needs the Bun runtime; use the npm packages". Replay of stored walks, which is what a pinned-binary consumer does, does not.

## Cut a release (only when Sean asks)

1. Set `version` in `apps/cli/package.json` and merge it to `main` through a pull request.
2. Tag the merge commit. The tag must be `v` plus that version, and the tagged commit must be on
   `main`, or the workflow stops.

   ```sh
   git fetch origin && git tag v0.1.0 origin/main && git push origin v0.1.0
   ```

3. The [Release workflow](../.github/workflows/release.yml) runs on the tag push (or on manual
   dispatch with the tag name). On a macOS arm64 runner it builds both binaries, runs the smoke
   check on the macOS one, runs `scripts/validate-release-artifacts.ts` and creates a **draft**
   GitHub release with the binaries and `SHA256SUMS`.
4. Open the draft, check the files and `SHA256SUMS`, then publish it by hand.

The workflow runs only on a pushed `v*` tag or manual dispatch, never on pull requests or pushes to
`main`. Only its one job has `contents: write`.

## Build and check locally

```sh
bun scripts/build-binary.ts                    # dist-bin/ (gitignored): both targets, .sha256 files, SHA256SUMS
bun scripts/smoke-binary.ts dist-bin/deja-<version>-darwin-arm64
```

The smoke check runs the binary with only `PATH=/usr/bin:/bin` and `HOME` set, so Bun is not on
`PATH`. It uses a temporary `DEJA_HOME`, the fake Maestro and the fake OpenClaw fixtures. It checks
`--version`, `--help`, `doctor --json`, the refusal of `explore`, `remember` of a candidate flow, a warm
`run` with zero model calls, and `list`.
Delete `dist-bin/` when you are done: it holds about 190 MB.

## Pin a version and checksum (consumers)

Record the version and the sha256 of your platform's binary in your repo, the way you pin Maestro
or Xcode. Take the checksum from `SHA256SUMS` on the release page.

```sh
DEJA_VERSION=0.1.0
DEJA_SHA256=<sha256 of deja-0.1.0-darwin-arm64>
```

Bump both together, in a pull request, when you adopt a new release.

## Use it from a bash host

[`docs/examples/install-deja.sh`](examples/install-deja.sh) downloads the pinned binary, checks the
sha256 before it becomes executable, caches it and prints its path. It fails on a mismatch.

```sh
deja="$(DEJA_VERSION=0.1.0 DEJA_SHA256=<sha256> docs/examples/install-deja.sh)"
"$deja" --version
"$deja" run "smoke" --config deja.config.json --summary
```

`--summary` makes the last stdout line a `deja-summary` line a script can parse. Exit codes are in
`deja --help`. Use a separate `DEJA_HOME` if you do not want the pinned Maestro engine in
`~/.local/share/deja`, and run `"$deja" engine install` once to fetch it.

The real run against a consumer flow on a simulator (Blankaday's `smoke.yaml`) is checked by that
consumer, not by this repository's CI.
