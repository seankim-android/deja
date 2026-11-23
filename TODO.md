# Déjà TODO

## Done

Submitted on Nov 22, 2026: [X post](https://x.com/kssc0112/status/2124667198837059953), [r/cerebras post](https://www.reddit.com/r/Cerebras/comments/1y34xuv/i_made_kimi_k31_pro_on_cerebras_test_my_app_once/), and the [Discord #share-your-demo post](https://discord.com/channels/1085960591052644463/1574227471706886236) (General Demo).

Tickets WALK-001 to WALK-014 and WALK-016 to WALK-020 are complete. The backlog lives in the Linear team DEJA. Evidence stays in the repo (`spikes/*/evidence/`, `demo/live-evidence/`); the old `tickets/` files are in git history before their removal.

## Open

- [ ] Make Déjà a consumable SDK: see [docs/SDK_PLAN.md](docs/SDK_PLAN.md). Open work is tracked in the Linear team DEJA; Blankaday is the first consumer.

## Standing rules

- Never save an unverified candidate or weaken a product assertion.
- Never auto-commit a repair or hide a real product regression behind a selector or source patch.
- Keep `.deja/`, build, temporary, credential, and personal data out of Git.
- Only the DEJA-16 publish process may publish a package. Never run `npm publish` or `bun publish` by hand. Each published package has a `prepublishOnly` guard (`scripts/publish-guard.ts`) that refuses unless `DEJA_PUBLISH_PROCESS=DEJA-16` is set, the git tree is clean and `dist/` was just built. Publish only with `bun publish`, never `npm publish` (npm would ship `workspace:*` ranges), and never with `--ignore-scripts` (that skips the guard). The guard also refuses unless `npm_config_user_agent` starts with `bun/`.
- Make no speedup claim without new raw evidence from equivalent full lifecycles.
