# Déjà — Agent Working Rules

Déjà is an Apache-2.0 memory layer and native runner for `maestro/v1` app tests across iOS, Android, and Web. The Rust workspace lives under [`rust/`](rust/Cargo.toml) alongside frozen TypeScript packages during the migration.

## Architecture & Boundaries

- Enforce the crate DAG via [`rust/crates/deja-cli/tests/workspace_boundaries.rs`](rust/crates/deja-cli/tests/workspace_boundaries.rs):
  - `deja-core` stays pure (closed-world direct dependency allowlist; zero driver, SQLite, HTTP, async runtime, or subprocess dependencies).
  - `deja-store` owns SQLite (`rusqlite` `bundled`); `deja-driver-web` owns WebSockets (`tokio-tungstenite`); `deja-cli` is the sole composition root (`clap`, `tracing-subscriber`, concrete drivers, `deja-mcp`).
  - No cross-driver edges (`deja-driver-{ios,android,web}` depend only on `deja-core` and `deja-flow`).
  - No OpenSSL or native-TLS crates anywhere in the workspace (`reqwest` uses `rustls`).
- Preserve 100% compatibility with `deja.walks.json` v1 (`closureSha256`, `assertionsSha256`), `deja-summary v=1`, and build fingerprint SHA-256 digests.
- Priority order for platform parity and live integration: **iOS -> Android -> Web**.
- Standard `maestro/v1` flow/driver behavior may reference [mobile-dev-inc/Maestro](https://github.com/mobile-dev-inc/Maestro) (Apache-2.0).

## Plan -> Execute Workflow

- For non-trivial tasks, plan first on Carbon (`pro` tier) and execute on Neon (`inherit`).
- Always verify locally before pushing:
  - Rust (from `rust/`): `cargo fmt --all -- --check`, `cargo fetch --locked`, `cargo clippy --locked --offline --workspace --all-targets --all-features -- -D warnings`, `cargo test --locked --offline --workspace --all-features`, and `DEJA_BOUNDARIES_ALL_TARGETS=1 cargo test --locked --offline -p deja-cli --test workspace_boundaries --all-features`.
  - Repo checks: `bun scripts/validate-release-artifacts.ts` (must pass on a clean committed tree; never leak local `/Users/...` home paths or secrets, and keep all local Markdown links valid).

## Required Worktree Workflow

- Work in a dedicated sibling worktree off freshly fetched `origin/main` (for example `<parent>/deja-wt/<ticket>`). Never edit the primary checkout unless asked; never commit or push directly to `main`; always open PRs against `main`.
- Never shut down or modify unrelated simulators/emulators or other worktrees.
- After confirmed merge, clean up the task worktree (and any detached review worktree) and remove any temporary simulator state created by the task.

## Cross-Review & Auto-Merge

- Open every PR as a draft against `main`: `gh pr create --draft --base main` (note the authoring agent in the PR body and link `Closes DEJA-<N>` when applicable).
- Run independent cross-review from a detached checkout of the pushed head (`git worktree add --detach <review-path> origin/<branch>`):
  - **Antigravity / Claude Code PRs** are reviewed by Codex (`codex review --base origin/main --title "<PR title>"`), capturing stdout, stderr, and exit status locally; keep raw diagnostic transcripts local and post the findings verbatim (never rewritten or summarized) after replacing local checkout paths with repository-relative paths and checking against the no-leak rule above.
  - **Codex PRs** are reviewed by Claude Code / Antigravity in a restricted read-only review pass on a detached checkout.
  - **Fallback on confirmed rate limit/outage only:** If `codex review` exits non-zero with a confirmed usage/rate limit or outage in captured stderr, run a fresh-context read-only self-review subagent (`pro` tier) on the detached checkout, post findings verbatim prefixed with `Self-review fallback (Codex limit/unavailable: <exact error>)`, and apply the same review gate.
- **Answer every finding** on the PR with the fix commit SHA or a linked `DEJA-` ticket for out-of-scope follow-ups, re-test, push, and re-run review on the new head.
- **3-round cap:** Run up to 3 review↔fix rounds. After a completed round 3, fix remaining in-scope findings, ticket deferred items, re-test, and reply with the verified fix SHA.
- **Auto-merge:** Once base is `main`, review is clean (<= 3 rounds, all findings fixed or ticketed), and all GitHub Actions CI checks pass on the exact head SHA:
  1. `gh pr ready <pr>`
  2. `gh pr merge <pr> --merge --match-head-commit <sha>` (merge commits only; no squash)
  3. Clean up task/review worktrees, mark the Linear `DEJA-` issue `Done`, and continue automatically onto unblocked follow-up tickets.
- Stop and ask Sean only for genuine user decisions or approval gates (costs, legal/privacy/trademark copy, public releases/tags, destructive migrations).
