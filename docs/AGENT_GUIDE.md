# Déjà Agent Guide

Déjà is a verified app test memory and native `maestro/v1` runner across iOS, Android, and Web. It verifies flows deterministically, saves their closure in a committed walk manifest (`deja.walks.json`), and replays them with **zero model calls**.

## Working Method

1. **Inspect before asking.** Run `deja doctor` (or `deja doctor --json`) first to detect Xcode/iOS simulators, `adb`/Android emulators, and Chromium. Report what is ready and name only the exact missing prerequisite — never retry-loop on manual steps.
2. **Verify before trusting.** Save a flow only after Déjà verifies it cleanly on its own (`deja remember <goal> --flow <path>`). Never edit `deja.walks.json` checksums by hand.
3. **Replay with zero model calls.** Run `deja run <goal>` (or `deja run --all` / `--devices <d1,d2>`) after code edits. Pin the target device per invocation with `--device <id>` or `DEJA_DEVICE=<id>` so parallel agents never clobber shared config.
4. **Gate releases deterministically.** Run `deja manifest check` for static closure validation (needs no device or build on disk) and `deja gate` to confirm every required goal passed on the current build fingerprint.
5. **Respect product assertions.** A broken UI selector may receive a scoped repair proposal, but a failing product assertion must stay failed — never weaken assertions to force a green run.

## Core Commands

```sh
deja doctor                                      # inspect local platform drivers and toolchains
deja manifest check                              # static check: flows match deja.walks.json
deja remember <goal> --flow <path.yaml>          # verify a flow cleanly and record its closure
deja run <goal> [--device <id>] [--verify]       # replay a remembered goal (0 model calls)
deja run --all [--devices <d1,d2>] [--junit out] # batch replay across one or more devices
deja gate --build <path> [--store <id>...]       # verify all required goals passed on this build
deja mcp --config deja.config.json               # expose confined tools over stdio JSON-RPC 2.0
deja guide                                       # print this embedded agent guide
```

## Platform Priority & Rules

- Priority order: **iOS Simulator → Android Emulator/Device → Web (Chromium CDP)**.
- Never leak local home paths (`/Users/...`) or credentials in committed flows, configs, or reports.
- Keep flows plain `maestro/v1` YAML committed in the app repository alongside `deja.walks.json`.
