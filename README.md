# WalkCache (Kimi Hackathon)

Explore once. Replay instantly. Repair only when needed.

WalkCache is a planned learning layer around Maestro: an OpenClaw agent explores a new QA
journey, saves a verified Maestro flow, and replays later runs without model calls. The current
repository is still a React and TypeScript starter; implementation status is tracked in
[`TODO.md`](TODO.md).

Read [`docs/PROJECT_PLAN.md`](docs/PROJECT_PLAN.md) for the MVP contract, architecture,
time-boxed milestones, demo script, and scope cut line.

## Quick start

Requirements: [Bun](https://bun.sh/) 1.4 or newer.

```sh
bun install
bun dev
```

Vite prints the local development URL. Edit `src/App.tsx` to start building.

## Commands

- `bun dev` — start the development server with hot reload.
- `bun run lint` — lint the source with Oxlint.
- `bun run build` — type-check and create a production build in `dist/`.
- `bun run preview` — preview the production build locally.

## Project layout

```text
src/       React application and styles
public/    Static assets served as-is
index.html Vite entry point
```

## Continuous integration

GitHub Actions runs dependency installation, linting, and the production build on every push and pull request to `main`.

## Environment variables

Keep credentials and local configuration in `.env.local`; those files are ignored by Git. Expose only non-secret values to the browser with Vite's `VITE_` prefix.
