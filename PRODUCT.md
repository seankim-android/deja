# Product

<!-- impeccable:product-schema 1 -->

## Platform

cli

## Users

Three audiences:

- Mobile developers on small teams who skip end-to-end QA because writing and maintaining tests costs too much.
- QA and test engineers who maintain Maestro suites that break when selectors change.
- Developers whose AI coding agents need a cheap way to check app flows via CLI or MCP (`DEJA-67`).

They work in a terminal, in CI and in a code editor, usually next to an iOS simulator, an Android emulator, or headless Chromium.

## Product Purpose

Déjà is an open-source verified app test memory and native `maestro/v1` runner built in Rust. A test comes from an AI model exploring the app once, or from YAML you already have, which needs no model. Déjà verifies that test by running it again on its own, and saves it only if every check passes. Later runs replay the saved test with zero model calls. A changed selector gets one small, scoped repair proposal; a real product bug still fails.

Success means a team owns plain test files that keep working across builds, with no model cost after the first run and no external JVM/Maestro runtime dependency.

## Positioning

Déjà is a memory layer and native runner for app testing. It learns a flow once, then remembers and replays it, and teams use one standalone Rust binary for all of that.

Learn once with a model, then replay with no model. Verification gates every saved test. Déjà can verify and save `maestro/v1` test YAML a team already has (`remember --flow`), including the subflows it calls.

## Operating Context

- Standalone pure-Rust Cargo workspace at repository root (`edition = "2024"`, MSRV `1.89`, `unsafe_code = "deny"`). The unpublished TypeScript/Bun prototype packages have been retired (`DEJA-75`).
- Native platform drivers in priority order: iOS Simulator (`xcrun simctl` + embedded Swift `DejaXCTestRunner`), Android (`adb` + bundled UiAutomator2 APK, zero host JDK), and Web (direct Chromium CDP over WebSocket, zero Node/Playwright).
- Optional cold exploration uses an OpenAI-compatible chooser (`DEJA-71`). Remember, replay, and gating work with zero model calls and no API key.

## Capabilities and Constraints

- The repository is Apache-2.0. The maintainer may later offer paid features under other terms (`DEJA-28`).
- Replay never needs a model or a provider key.

## Brand Commitments

- The name is always spelled **Déjà**, with both accents, in the wordmark and in prose. The ASCII form `deja` / `deja-run` is only for package names, domains and identifiers.
- The "déjà vu" idea is binding: the identity should evoke seeing something again, or a repeat.
- The mark must work as a CLI and terminal icon: monochrome, at small sizes, in README and terminal contexts, and as a 16px favicon.
- Original work only. No imagery from *The Matrix* or other Warner Bros. properties: no green digital or code rain, no black cat, no katakana columns.
- Formerly called WalkCache. That name and the "WC" placeholder favicon are retired and not brand assets.
- The trademark policy is in TRADEMARKS.md (draft, DEJA-27).
- GitHub org `deja-run`, domain `deja-run.com`.

## Evidence on Hand

- Demo GIF and narrated 55-second demo: `demo/deja-preview.gif`, `demo/deja-demo.mp4` (recorded on the retired prototype).
- Live native iOS Simulator proof (`DEJA-60`): `drivers/ios/DejaXCTestRunner/SPIKE_EVIDENCE.md`.
- Measured exploration results for Kimi K3.1 Pro on Cerebras only (`demo/live-evidence/`). No other model has been measured, and none should be claimed.
- No customers, testimonials or pricing exist yet. Don't invent any.

## Product Principles

1. Verify before trusting: nothing is saved unless it passes on its own.
2. Zero-model replay is the default path; the model is there for learning, not running.
3. Users own plain test files, and Déjà interoperates with existing setups instead of replacing them.
4. Be honest about limits. Unmeasured claims are labeled unknown.
5. Small, scoped repairs never hide a real product bug.
