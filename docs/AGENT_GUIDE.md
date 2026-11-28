# Déjà Agent Guide

Déjà is a verified app test memory and native `maestro/v1` runner across iOS, Android, and Web. It verifies flows deterministically, saves their closure in a committed walk manifest (`deja.walks.json`), and replays them with **zero model calls**.

## Working Method

1. **Inspect before asking.** Check local toolchains and device readiness first. Report what is ready and name only the exact missing prerequisite — never retry-loop on manual steps.
2. **Verify before trusting.** Save a flow only after Déjà verifies it cleanly on its own. Never edit `deja.walks.json` checksums by hand.
3. **Replay with zero model calls.** Replay remembered goals after code edits, pinning the target device per invocation (`--device` / `DEJA_DEVICE`) once the native CLI lands so parallel agents never clobber shared config.
4. **Gate releases deterministically.** Run static manifest checks before running device suites, and require every goal in the release gate to pass on the current build fingerprint.
5. **Respect product assertions.** A broken UI selector may receive a scoped repair proposal, but a failing product assertion must stay failed — never weaken assertions to force a green run.

## Native Rust Binary (Available Now)

During the Rust migration, the standalone Rust CLI exposes:

```sh
deja --version                                   # print binary version
deja --help                                      # list available native subcommands
deja guide                                       # print this embedded agent guide
deja-repo-check --require-clean --check-chart    # validate repository links, JSON, leaks & chart SVG
```

## Planned Native Rust Commands (`DEJA-65` / `DEJA-66` / `DEJA-67`)

The full native CLI surface below is tracked in Linear (`DEJA-65`..`DEJA-67`) and will become directly runnable on the Rust binary as each phase lands:

```sh
deja doctor [--json]                             # inspect local Xcode/simctl, adb, and Chromium readiness
deja manifest check                              # static check: flows match deja.walks.json
deja remember <goal> --flow <path.yaml>          # verify a flow cleanly and record its closure
deja run <goal> [--device <id>] [--verify]       # replay a remembered goal (0 model calls)
deja run --all [--devices <d1,d2>] [--junit out] # batch replay across one or more devices
deja gate --goals <file> [--store <id>...]       # verify all required goals passed on this build
deja mcp --config deja.config.json               # expose confined tools over stdio JSON-RPC 2.0
```

## Platform Priority & Rules

- Priority order: **iOS Simulator → Android Emulator/Device → Web (Chromium CDP)**.
- Never leak local home paths (`/Users/...`) or credentials in committed flows, configs, or reports.
- Keep flows plain `maestro/v1` YAML committed in the app repository alongside `deja.walks.json`.
