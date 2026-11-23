# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

Three audiences:

- Mobile developers on small teams who skip end-to-end QA because writing and maintaining tests costs too much.
- QA and test engineers who maintain Maestro suites that break when selectors change.
- Developers whose AI coding agents need a cheap way to check app flows. The MCP server is planned (DEJA-9).

They work in a terminal, in CI and in a code editor, usually next to an Android emulator or iOS simulator.

## Product Purpose

Déjà is an open-source SDK and CLI. An AI model explores an app once and writes a Maestro test. Déjà verifies that test by running it again on its own, and saves it only if every check passes. Later runs replay the saved test with zero model calls. A changed selector gets one small, scoped repair; a real product bug still fails.

Success means a team owns plain Maestro YAML tests that keep working across builds, with no model cost after the first run.

## Positioning

Learn once with a model, then replay with no model. Verification gates every saved test. Déjà also reads existing Maestro configuration and flows and interoperates with them, so teams can adopt it without rewriting what they already have.

## Operating Context

- CLI (`deja`), Bun workspaces and npm packages under the `@deja-qa` scope.
- Maestro 2.x on Android, iOS and Chromium web.
- Cold exploration runs through the OpenClaw harness. The default model is Kimi K3.1 Pro on Cerebras, and any OpenAI-compatible model can be configured.
- An optional local run viewer (`viewer/`).
- The backlog lives in the Linear team DEJA.

## Capabilities and Constraints

- The public packages are Apache-2.0 and form the free tier. A paid hosted tier is planned in a separate private repo; its features are undecided (DEJA-28).
- Replay never needs a model or a provider key.
- Undecided: the license for future releases and the business entity (DEJA-29).

## Brand Commitments

- The name is always spelled **Déjà**, with both accents, in the wordmark and in prose. The ASCII form `deja` / `deja-qa` is only for package names, domains and identifiers.
- The "déjà vu" idea is binding: the identity should evoke seeing something again, or a repeat.
- The mark must work as a CLI and terminal icon: monochrome, at small sizes, in README and terminal contexts, and as a 16px favicon.
- Original work only. No imagery from *The Matrix* or other Warner Bros. properties: no green digital or code rain, no black cat, no katakana columns.
- Formerly called WalkCache. That name and the "WC" placeholder favicon are retired and not brand assets.
- The trademark policy is in TRADEMARKS.md (draft, DEJA-27).
- npm scope `@deja-qa`, domain deja-qa.com.

## Evidence on Hand

- Demo GIF and narrated 55-second demo: `demo/deja-preview.gif`, `demo/deja-demo.mp4`.
- Measured results for Kimi K3.1 Pro on Cerebras only. No other model has been measured, and none should be claimed.
- No customers, testimonials or pricing exist yet. Don't invent any.

## Product Principles

1. Verify before trusting: nothing is saved unless it passes on its own.
2. Zero-model replay is the default path; the model is there for learning, not running.
3. Users own plain Maestro YAML, and Déjà interoperates with existing setups instead of replacing them.
4. Be honest about limits. Unmeasured claims are labeled unknown.
5. Small, scoped repairs never hide a real product bug.
