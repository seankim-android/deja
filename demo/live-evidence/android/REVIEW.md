# WALK-020 Android live-capture review

## Accepted capture set

Run ID: `walk020-android-checkout-06`. Only `walkcache_api_34` / `emulator-5554` (API 34) was used.
Each phase has a continuous on-device `adb screenrecord` take and captured production CLI receipt/report.
Raw uncut MP4s stay under ignored `demo/generated/android/walk020-android-checkout-06/`.

### Fresh cold/warm lifecycle

- Cold `explore checkout`: PASS, `cold-recorded`, 13 model round trips, 14 assertions, 0 repairs. Real
  Maestro MCP exploration was followed by an independent fresh-clear-state verification through the exact
  `Order #WC-1001 · Mochi sticker pack` assertion. Only after that PASS was the YAML atomically stored.
- Warm `run checkout`: PASS, exact cache hit, 14 assertions, 0 model calls, 0 repairs. Its complete cache
  identity matches cold exactly. Both YAML files are byte-identical with SHA-256
  `e95532ce04a4e125d3bd56e1b9d5d367b31dd67eed4441c56271ca52f3c3d0a1`.
- This model-authored flow uses short selectors and asserts the checkout ID before tapping it. It is used
  only for the fresh cold/warm lifecycle; it was never represented as eligible for the one validated
  repair diagnosis and was not edited to make it eligible.

### Explicitly separate repair lifecycle

- A byte-identical copy of the independently verified model-authored candidate from
  `walk020-android-20261122T200956-97974` was supplied to production `remember checkout`. Its source
  exploration used 12 model round trips; the direct checkout tap uses the fully qualified resource ID.
  `repair-fixture-provenance.json` records this separate source lifecycle.
- `remember` independently passed all 12 assertions from fresh clear state through the unchanged final
  assertion, then atomically stored the candidate. Baseline YAML SHA-256 is
  `2b863bb62021ec93370ada0a626c90f8b6ee07a8a380c545589f81237060e76f`; no bytes changed.
- `repair-warm` then passed as an exact baseline identity/YAML hit with 12 assertions, 0 model calls and
  0 repairs. This warm hit belongs to the repair fixture, not to the distinct 14-assertion fresh cold flow.
- The driver preflight confirmed that the first checkout reference is the exact validated
  `Tap on id: dev.walkcache.checkout:id/checkout_button`. It did not edit an assertion or broaden the
  classifier. Only then did it reverse the established experiment-only two-file patch, build/install the
  stale APK, and invoke production `run`.
- Diagnosis failed at that exact checkout tap with category `selector` and `repairEligible: true`. One
  bounded attempt (240,000 ms budget; 66,879 ms CLI duration) changed only `MainActivity.java` and
  `ids.xml`, with no auto-commit or pull request. Build passed in 689 ms; independent verification passed
  all 12 assertions in 25,519 ms; atomic persistence preserved YAML/assertions byte-for-byte.
- The stale-run receipt retains input fingerprint
  `6498293ae1d610dee34d0d5719e05032edab132062302cc374a638503c3b6ae0`. The repaired build's recomputed
  fingerprint is `1de8864ea046b64d124ece93ca97b0ae69d11d1fc3d39a89ff405f0e2dd2262d`; final replay hit
  that repaired-cache identity with 12 assertions, 0 model calls and 0 repairs.

The distinct YAML hashes are intentional and visible on the video panels. No receipt is presented as an
identity match across these two separate lifecycles.

## Raw video validation

| Phase | Raw duration | Fully decoded frames | Raw duplicate-DTS messages |
| --- | ---: | ---: | ---: |
| cold | 99.032822 s | 1,348 | 6 |
| warm | 26.986933 s | 411 | 0 |
| repair-baseline | 27.656978 s | 398 | 48 |
| repair-warm | 26.438556 s | 397 | 4 |
| stale-repair | 68.125700 s | 869 | 51 |
| replay | 27.041022 s | 404 | 7 |

Every raw take has a nonzero H.264 video stream, matching SHA-256 sidecar, successful FFprobe frame count,
and complete FFmpeg decode with exit code 0. Some emulator recordings contain duplicate presentation/DTS
messages around static frames; these are recorded above rather than hidden. Decode still consumed the full
frame counts. The edited segments normalize timestamps to constant 30 fps and decode successfully without
those raw timestamp warnings. `evidence.json` retains each raw probe and warning count.

The concise segments uniformly time-compress stated raw ranges; they do not selectively remove waits or
failures within those ranges. The visible playback factors are approximately 8.3× cold, 3.9× fresh warm,
6.0× before repair, 2.9× after repair, and 4.5× final replay. This is edited playback, not a wall-clock
speedup claim. Exact ranges, hashes and frame counts are in `android-edit-map.json`.

## AI visual review and corrections

A vision-capable `view_image` review inspected the raw contact sheet, raw cold confirmation at 98.5 s,
and full-resolution composite segment frames. Initial findings were used to correct the presentation, not
the underlying evidence:

- cold was lengthened to a 12-second target so its actual final confirmation is readable near the end;
- the Android overlay now fits inside the panel and no longer covers the `REAL ANDROID UI` label;
- the stale panel explicitly says `checkout_button ID missing`, distinguishing the visible Checkout button
  from its absent resource ID;
- separate repair fixture and repaired-fixture replay titles are visible;
- `EXPECTED FINAL ASSERTION` on cold/before-repair and `FINAL PRODUCT ASSERTION` after a visible PASS,
  plus all playback footers, are readable and unclipped.

The corrected full-resolution review passed all six inspected Android frames: cold start/end, warm end,
before repair, after repair end, and green replay. It confirmed real launcher/login/cart/confirmation UI,
visible `Order #WC-1001 · Mochi sticker pack` at cold/warm/after-repair/replay ends, intact phone/navbar
bounds, separate YAML prefixes, readable playback factors, and no private data or blank frames. A
question about `0 model calls` during repair was checked against the actual receipt basis: repair used the
verified deterministic policy only; no model was invoked. This is AI review, not human sign-off.

## Restoration, redaction and rejected takes

Normal selector source hashes match baseline exactly; `selector-restoration.diff` is empty and no
`.orig`/`.rej` files remain. Global `walkcache_checkout_selector_stale` and
`walkcache_product_regression` are both `null`. The final replay is green on the normal selector state.
The WALK-014 fallback remains SHA-256
`3e6c9ec2f4040ec1a8e8bb1e4f8eee2bbe977d6e3f2a69e5b6503cf2f178e461`.

The curated stale-diagnosis log preserves the failed tap, classifier context, and every assertion result
but replaces the local macOS Maestro debug directory with `[private local Maestro debug path redacted]`.
The ignored raw log is unchanged. Curated text passed private-path, common-token, and authorization scans.
Only synthetic fixture credentials appear.

Earlier cold/repair attempts—including the wrong-goal rejection, the unknown-classifier refusal, and
short-ID/extra-assertion cold takes—are documented in [REJECTED.md](REJECTED.md). They were not silently
repaired by deleting assertions or broadening the configured policy.
