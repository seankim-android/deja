# Portable checks and local platform gates

## CI: Linux, no devices, no model

[`.github/workflows/portable-checks.yml`](../.github/workflows/portable-checks.yml) runs on `main`
pushes, pull requests, and manual dispatch on `ubuntu-latest` with read-only repository permissions.
The job checks out the exact commit, installs Bun 1.4.4, then runs:

```sh
bun install --frozen-lockfile
bun run lint
bun test --timeout=10000
bun run build
bun scripts/validate-release-artifacts.ts
git rev-parse HEAD
```

The 101 deterministic unit/component/contract tests cover orchestration, exact/corrupt storage,
runner behavior and platform adapters, OpenClaw exploration boundaries, bounded repair, failure
classification, cold/warm metrics, CLI/report behavior, and the local viewer. They use fake process
fixtures where appropriate and do not assert a real device result.

The artifact validator parses every strict tracked JSON file (compiler-owned JSONC `tsconfig` files are
separately validated by `bun run build`), rejects generated cache/build/dependency/binary artifacts, scans text for private keys/token shapes and machine-specific home paths, and fails if the
checkout changed during checks. Generated demo scene PNGs remain ignored; the final MP4, generator,
source text, and evidence-linked rehearsal results are curated intentionally.

## Reproduce a clean checkout

```sh
git clone https://github.com/seankim-android/kimi-hackathon.git
cd kimi-hackathon
bun install --frozen-lockfile
bun run lint
bun test --timeout=10000
bun run build
bun scripts/validate-release-artifacts.ts
git status --short
git rev-parse HEAD
```

No ignored cache, emulator state, credentials, build output, or generated viewer assets are required
for these checks. `scripts/validate-release-artifacts.ts` must run against the committed clean tree.

## Platform integration is intentionally not a portable CI claim

Android emulators, OpenClaw/Kimi provider credentials, and Maestro's experimental Chromium driver are
not available to this portable job. CI does **not** relabel fake runner tests as real Android/Web runs.
The actual local executions remain separately curated:

- Android: [`WALK-010 compatibility replay`](../spikes/001-android-walkcache/evidence/walk010-android-warm.txt)
  and [`README quick-start review`](../spikes/001-android-walkcache/evidence/walk012-readme-review.txt).
- Chromium: [`warm replay`](../spikes/002-web-walkcache/evidence/walk010-warm.txt),
  [`independent remember`](../spikes/002-web-walkcache/evidence/walk010-remember.txt), and
  [`regression`](../spikes/002-web-walkcache/evidence/walk010-product-regression.txt).
- iOS is not claimed and has no CI/integration entry.

To verify that stored Android YAML is standalone Maestro rather than WalkCache-dependent syntax,
start the deterministic Android fixture as described in the root README, then from its directory run:

```sh
. ./scripts/env.sh
sh scripts/fixture-mode.sh stable
maestro --no-ansi --device emulator-5554 test evidence/repaired-checkout.yaml
```

For the Web standalone command and its fixture server, see
[`spikes/002-web-walkcache/README.md`](../spikes/002-web-walkcache/README.md). Maestro 2.12.0 labels
Web Beta/Experimental; `--device chromium` and a `url:` flow declaration are required. Cold Web
exploration and Web repair are intentionally unsupported. No platform command needs a viewer, and
`remember`/warm replay need no model access.
