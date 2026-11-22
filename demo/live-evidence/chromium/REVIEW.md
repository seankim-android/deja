# WALK-020 Chromium live-capture review

## Accepted pair

Run ID: `walk020-20261122T195607-94392`.

- Raw uncut `remember`: `demo/generated/browser/walk020-20261122T195607-94392/remember.mp4`,
  19.733333 s, 592 frames, 45 actual Chromium screencast events, SHA-256
  `e4716b3afa5d005b387c147ea425db77c12bb5fa2f65dba67d02885e47d198ef`.
- Raw uncut warm `run`: `demo/generated/browser/walk020-20261122T195607-94392/run.mp4`,
  18.900000 s, 567 frames, 44 actual Chromium screencast events, SHA-256
  `122deab0709de1d9c58166210b2245206320de46db4df95347772606848a0fbe`.
- Both videos are H.264 and passed FFprobe frame counting and complete FFmpeg decode with no errors.
- `remember` passed independent verification and persisted the reviewed YAML only after 12 assertions
  passed with 0 model calls. Warm `run` passed 12 assertions, exact cache hit, 0 model calls, 0 repairs.
- The receipts' full cache identities match exactly. Both saved YAML files and the reviewed candidate
  have SHA-256 `944d55991d28716f3da90e71546227a946cd02b02c88ed52edb62600e2167aff` and retain the exact
  final assertion `Order WC-1001 · Mochi sticker pack`.

## Capture truthfulness

Maestro ran headed with `--device chromium` and no `--headless` argument. The recorder attached to the
same isolated Chrome process's loopback DevTools endpoint and consumed its real `Page.screencastFrame`
stream. Static waits hold the latest actual browser frame at 30 fps; the sidecars record every source
frame event and held output frame. This is continuous browser-backed recording, not desktop capture
and not a journey fabricated from independently staged screenshots.

macOS AVFoundation listed no video devices, both capture indexes returned `Invalid device index`, and
`screencapture` returned `could not create image from display`. See `capture-permission.txt`. No
desktop/window-capture claim is made.

The concise browser segments uniformly play the complete raw takes at 2.5×: 7.866667 s before cache
and 7.533333 s after cache. No action, wait, failure, or assertion frames were selectively removed.
The visible panels state `PLAYBACK 2.5×` and `edited playback only, not a speedup claim`. Raw CLI
process durations are retained separately; no wall-clock comparison is claimed.

## Visual and content review

AI visual inspection of `contact-sheet.png` confirmed the real loading, fixture email entry, masked
fixture password, cart, checkout, and final order states. Separate full-resolution inspection of
`demo/generated/review/browser-before-panel.png` and `browser-after-panel.png` confirmed:

- legible `BEFORE CACHE` / `AFTER CACHE`, command, assertion/model-call, YAML hash, and compression labels;
- actual browser UI remains visible alongside the report panel rather than being replaced by a slide;
- the same deterministic order `WC-1001` appears after both journeys;
- only the documented fixture values `qa@walkcache.dev` / masked `testing123` are present, not real
  credentials or personal data;
- no private absolute paths, tokens, or unrelated desktop content appear.

The curated text evidence also passed automated scans for private absolute home/temporary paths,
common token formats, and authorization material. Full video decode passed for both concise segments
(236 and 226 frames).

A second full-resolution vision review checked panel bounds and corrected the presentation: the browser
overlay now fits at 880 px inside its left panel; `WalkCache · real headed Chromium` is visible in the
right title; the left footer explicitly says CDP screencast, not desktop capture; and the right playback
footer has safe frame margins. The final BEFORE/AFTER frames both show real `Order WC-1001 · Mochi
sticker pack` confirmation, matching cached YAML hashes, readable 2.5× disclosures, and no private paths,
secrets, clipping, or blank frames. This records automated and AI review, not human sign-off.

## Rejected take

Run ID `walk020-20261122T195407-93117` remains under ignored generated storage. Its real headed MP4
was captured, but the orchestrator gave the CLI receipt and screencast sidecar the same `remember.json`
filename. Concurrent writes made the combined receipt invalid, so the take was rejected rather than
curated or repaired after the fact. The recorder now writes `*.screencast.json`, CLI output writes
`*.receipt.json`, recorder stdout writes `*.recorder-summary.json`, and each phase has separate
`*.session.json`. The accepted pair above was recorded fresh after that correction.

## Existing fallback

`demo/walkcache-demo.mp4` remains unchanged at SHA-256
`3e6c9ec2f4040ec1a8e8bb1e4f8eee2bbe977d6e3f2a69e5b6503cf2f178e461`. The combined live demo later
passed final automated and independent AI review as `demo/walkcache-live-demo.mp4` (59.733333 seconds,
SHA-256 `d834bda3b156137df2693d11297855180e8e5c681fc870417fa05a0647aec945`); human sign-off remains
unclaimed.
