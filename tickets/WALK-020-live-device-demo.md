---
id: WALK-020
title: Record reviewed live Android and Chromium before/after demo
milestone: M7C
priority: P0
status: done
depends_on: [WALK-014]
---

# WALK-020 — Record reviewed live Android and Chromium before/after demo

## Outcome

Replace the static-evidence demo for the next authorized demo upload with actual uncut device/browser
recordings and a concise, clearly mapped live cut. Preserve the reviewed WALK-014 fallback until this
new recording passes its review gate. This ticket authorizes demo capture and Slack thread delivery
only; it does not authorize a public social post or official hackathon submission.

## Resource gates

- Chromium/local Web fixture work is authorized first, using only deterministic fixture credentials.
- Do not boot, launch, stop, use, or alter any Android emulator, app, or cache until the requester
  sends the exact message `ANDROID_GO`. Another independent review owns that resource until then.
- Do not interfere with the active iOS fixture or simulator. Preserve unrelated iOS working-tree
  changes and never stage them.

## Checklist

- [x] Record real headed Maestro `--device chromium` `remember` and warm `run`, retaining uncut
  browser-backed video, captured CLI output/reports, independent verification, and saved YAML.
- [x] Prove the Chromium warm report is an exact cache hit with 0 model calls and the same meaningful
  final product assertion. Label the live segments `BEFORE CACHE` and `AFTER CACHE`.
- [x] After `ANDROID_GO`, capture continuous emulator UI for cold exploration/verification, warm
  replay, stale checkout selector failure, configured bounded repair, unchanged final assertion,
  and green replay. Label repair segments `BEFORE REPAIR` and `AFTER REPAIR`.
- [x] Preserve and restore the validated Android fixture and normal selector state. Never weaken an
  assertion, hide a regression, auto-commit a repair into fixture code, or claim wall-clock speedup.
- [x] Keep raw uncut MP4 files in ignored `demo/generated/`; clip Android captures when the platform
  duration limit requires it. Retain curated secret-free commands/reports in Git.
- [x] Produce a reviewed live MP4 of at most 60 seconds, with explicit time compression/omitted waits
  where used and a mapping from every segment to raw timestamps and reports.
- [x] Validate every delivered video with ffprobe, full decode, visual review, and secret/private-path
  scans. Do not imply desktop/window capture when OS permission blocks it.
- [x] Keep `demo/walkcache-demo.mp4` unchanged as the WALK-014 fallback until the new live cut passes
  review; upload only the completed reviewed live MP4 to Slack channel `C0CKR1JT42Z`, thread
  `1795305727.220879`, and record its file ID.
- [x] Commit only this ticket's scripts, curated evidence, documentation, and reviewed video after
  checks; inspect the index and push the resulting commit to `main`. Create no PR.

## Plan

1. Add this tracking ticket, TODO/index links, and ignored generated storage; leave all existing demo
   assets and unrelated application changes untouched.
2. Probe actual AVFoundation capture. The initial probe lists no video devices; both screen-device
   indexes fail with `Invalid device index`, and `screencapture` reports `could not create image from
   display`. Treat this as an OS screen-recording permission blocker unless a later direct probe
   succeeds. Use a genuine browser video stream as the fallback, never a slideshow assembled from
   screenshots, and label its capture scope accurately.
3. Run the loopback Web fixture and real Maestro headed Chromium. Record `remember` before cache and
   exact-hit `run` after cache, with separate raw UI and CLI evidence. A browser-backed recording is
   evidence of rendered fixture activity; it is not claimed to be desktop capture.
4. Pause before Android until the exact `ANDROID_GO` continuation. Then boot/use only
   `walkcache_api_34` / `emulator-5554`; build/install the validated fixture and select stable mode.
   Record a fresh production `explore` cold lifecycle (real model exploration plus independent
   verification), then `run` against the exact same identity. Because several independently passing
   model-authored flows did not match the repairer's one validated fully qualified tap diagnosis, keep
   that fresh cold/warm lifecycle explicitly separate from repair. Revalidate a hash-preserved direct-tap
   model-authored candidate with production `remember checkout`, then record its own exact repair-warm
   hit. Before mutation, require the exact `Order #WC-1001 · Mochi sticker pack` assertion and validated
   tap in that repair-fixture YAML. Back up and hash-check `MainActivity.java` and `ids.xml`, reverse only
   `evidence/walk007-repair.diff`, rebuild and install, then record production `run` performing
   selector-only diagnosis and one bounded repair. Require one attempt, the same two configured files,
   unchanged YAML/final assertion within the repair lifecycle, rebuild plus independent verification,
   and a final zero-model green replay on the recomputed build identity. Restore exact baseline source
   bytes on any failure, never commit the repair, select stable device mode, and leave the normal fixture
   passing. Use continuous adb screenrecord, starting another clip immediately when Android's 180-second
   limit is reached; raw clips and CLI attempts remain ignored.
5. Add repeatable edit and validation scripts, a concise live script/segment map, curated reports, and
   two end-to-end reviews. Do not use editing to conceal failures or imply a speed comparison.
6. After all gates pass, upload the reviewed MP4 through the authorized Slack message tool, record
   the Slack file ID, stage only named WALK-020 files, inspect staged diff/status, commit, and push
   `main`. No external social post or official submission.

## Planned files and verification

- `demo/live/`: capture/edit/validation procedures and scripts, using installed tools only.
- `demo/generated/`: ignored uncut recordings, intermediate streams, and review frames.
- `demo/live-evidence/`: curated relative-path commands, reports, YAML, hashes, timing map, and reviews.
- `demo/LIVE_SCRIPT.md` and `demo/walkcache-live-demo.mp4`: reviewed concise cut and evidence map.
- Verify real command exit codes and report fields; ffprobe container/codec/duration/frame counts;
  full FFmpeg decode; sampled and segment-boundary visual review; complete automated scans for
  credentials, tokens, private absolute paths, unsupported claims, and stale fallback modification.

## Acceptance evidence

Chromium, Android, the combined cut, final automated/AI review, authorized Slack delivery, and durable
delivery receipt are complete:

- [Curated commands, receipts, saved YAML, capture metadata, edit map, and review](../demo/live-evidence/chromium/)
  for accepted run `walk020-20261122T195607-94392`. Raw uncut videos stay under
  `demo/generated/browser/walk020-20261122T195607-94392/`.
- `remember`: independent verification passed 12 assertions with 0 model calls and persisted reviewed
  YAML `944d55991d28716f3da90e71546227a946cd02b02c88ed52edb62600e2167aff`.
- Warm `run`: exact same identity/YAML, cache hit, 12 assertions, 0 model calls, 0 repairs, and unchanged
  `Order WC-1001 · Mochi sticker pack` assertion.
- Raw videos: 19.733333 s / 592 frames and 18.900000 s / 567 frames, both H.264, full decode passed.
  Concise browser segments are 7.866667 s and 7.533333 s at a visible 2.5× playback-compression label,
  with no removed waits/failures and no wall-clock speedup claim.
- [AI visual/content review and rejected-take record](../demo/live-evidence/chromium/REVIEW.md):
  accepted frames are readable and secret-free; human sign-off is not claimed. macOS desktop capture
  permission was blocked, so the truthful capture is the exact headed Maestro Chromium CDP screencast.
- WALK-014 fallback SHA-256 remains `3e6c9ec2f4040ec1a8e8bb1e4f8eee2bbe977d6e3f2a69e5b6503cf2f178e461`.

## Android and final-cut evidence

- [Android review](../demo/live-evidence/android/REVIEW.md) and [rejected-take record](../demo/live-evidence/android/REJECTED.md)
  cover accepted raw run `walk020-android-checkout-06` and all rejected/restricted takes.
- Fresh cold/warm: 13 real model round trips, independent verification, then exact identity/YAML hit with
  0 model calls; 14 assertions and YAML `e95532ce04a4e125d3bd56e1b9d5d367b31dd67eed4441c56271ca52f3c3d0a1`.
- Separate repair fixture: byte-preserved model-authored direct-tap YAML independently revalidated with
  `remember`; its own warm hit uses 12 assertions and YAML
  `2b863bb62021ec93370ada0a626c90f8b6ee07a8a380c545589f81237060e76f`.
- One bounded repair: selector classification, two configured files only, 0 model calls, independent
  rebuild/verification, atomic persistence, unchanged YAML/final assertion, and final 0-model green replay
  on fingerprint `1de8864ea046b64d124ece93ca97b0ae69d11d1fc3d39a89ff405f0e2dd2262d`.
- All raw takes pass hash checks, FFprobe frame counting and full decode. Raw emulator duplicate-DTS
  messages are quantified in the review; the normalized final MP4 decodes with no warnings/errors.
- Normal selector source hashes and stable device flags are restored; source diff is empty, no patch
  backups remain, and the WALK-014 fallback is unchanged.
- Final video: [`demo/walkcache-live-demo.mp4`](../demo/walkcache-live-demo.mp4), 59.733333 seconds,
  1920×1080, 30 fps, 1,792 decoded frames, SHA-256
  `d834bda3b156137df2693d11297855180e8e5c681fc870417fa05a0647aec945`.
- [Final visual/decode/raw-map review](../demo/live-evidence/FINAL_REVIEW.md),
  [final cut map](../demo/live-evidence/live-cut-map.json), and
  [read-only final gate](../demo/live-evidence/final-check.json) all pass. The final gate rehashed and
  decoded raw sources, verified all seven evidence segments and scanned 72 curated text files. Human
  sign-off is not claimed.

The requester uploaded the exact reviewed MP4 to Slack channel `C0CKR1JT42Z`, thread
`1795305727.220879`, as file `F0CL4RMSE4C`. The durable [Slack receipt](../demo/live-evidence/slack-upload.json)
matches its SHA-256. [Final delivery gate](../demo/live-evidence/final-delivery-check.json):
`python3 demo/live/check_live_demo.py --check-raw --require-upload` passed, including Slack receipt
verification, raw hash/decode checks, all seven evidence segments, and scans of 74 curated text files.
Human sign-off is not claimed. No external social post or official submission was made.
