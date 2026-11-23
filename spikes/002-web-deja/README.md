# Déjà deterministic Web fixture

A local, secret-free login → add item → checkout journey used only by WALK-010. It is not the
Déjà viewer and does not share code with the validated Android fixture.

## Run

From this directory, with the Android fixture's environment helper used only for Maestro/Java `PATH`:

```sh
bun server.ts
# In another shell:
. ../001-android-deja/scripts/env.sh
bun ../../apps/cli/deja.ts remember checkout \
  --flow flows/checkout.yaml --config deja.config.json
bun ../../apps/cli/deja.ts run checkout --config deja.config.json
```

The server binds only to `127.0.0.1:4174` by default and serves `public/` with no caching. Both
fixture fields are deterministic test values; never substitute personal credentials. `appId`/the
YAML `url` is the loopback fixture origin. The Web adapter fingerprints relative paths and contents
of every regular file under `public/`; it does not fingerprint only HTML while ignoring JavaScript.

## Verified support boundary

Installed **Maestro 2.12.0** labels Web support **Beta** and names its device **Chromium Desktop
Browser (Experimental)**. The `test` command exposes Web-only `--headless` and `--screen-size`
options. A Web flow starts with `url:` (not Android `appId:`) and uses `--device chromium`; omitting
it produces `Not enough devices connected` even though `maestro --platform web list-devices` lists
Chromium. The direct command used here is:

```sh
maestro --no-ansi --platform web --device chromium \
  test --headless --screen-size 1280x800 flows/checkout.yaml
```

| Behavior | WALK-010 status |
| --- | --- |
| Real Chromium login → add item → checkout with 12 assertions | Supported and verified |
| `remember` independently verifies before persisting | Supported and verified |
| Exact warm replay without a model call | Supported and verified |
| Cache partition by goal/app/platform/flow schema/adapter/browser/frontend build | Supported and tested |
| Frontend static-build change | Invalidates; repair is rejected, with no blind cold fallback |
| Backend response regression with unchanged frontend build | Real assertion failure, no repair |
| Cold model-led Web exploration | **Unsupported**: exits as rejected with 0 model calls; use reviewed YAML + `remember` |
| Web selector repair | **Unsupported**: Android repair is never reused for Web |
| Firefox, Safari, non-loopback targets, parallel shards, or mobile web | Not claimed |

The adapter identity is `maestro-web@2.12.0`, browser identity is `chromium`, and both are persisted
in reports and exact local cache identity. Existing Android cache entries retain their legacy
identity and commands.

For the regression proof only, restart the same deterministic backend with
`DEJA_FIXTURE_ORDER=WC-9999`. The frontend bytes and build fingerprint stay unchanged, but
`GET /api/order` returns a different order number. The cached flow must fail its final exact order
assertion. Restore the default `WC-1001` backend and independently `remember` the known-good YAML to
return to a verified pass; failed validation is never presented as a warm success.

See the curated `evidence/walk010-*.txt` transcripts and unit/integration tests for actual results.
No wall-clock reduction, generalized Web exploration, or cross-browser support is claimed.
