# WalkCache local run viewer

An optional, read-only UI for local run reports. It never runs exploration, replay, repair, or a model.
The headless CLI remains fully functional when this viewer is stopped.

```sh
bun run dev -- --host 127.0.0.1 --port 4173
```

Open `http://127.0.0.1:4173/`. The fixture gallery links `?fixture=cold`, `warm`, `repair`, and
`regression` use deterministic fixture data only and do not read workspace files. Workspace reports
come from `.walkcache/reports/` in the directory where Vite was started.

The Vite middleware exposes only three local `GET` endpoints: run index, validation history, and
report-declared artifacts. It has no command or write endpoint. YAML, JSON, text, and diff paths must
be declared by a valid report, stay under `.walkcache`, pass canonical-path/symlink checks, and avoid
credential-like names. Raw logs are not viewable. Responses are independently redacted.

Run the focused tests with:

```sh
bun test src/App.test.tsx viewer/test/local-viewer.test.ts
```

CLI fallback (from a configured fixture workspace):

```sh
bun ../../apps/cli/walkcache.ts list --config walkcache.config.json
bun ../../apps/cli/walkcache.ts inspect .walkcache/reports/latest.json --config walkcache.config.json
```
