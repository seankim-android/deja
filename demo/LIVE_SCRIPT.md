# WalkCache — reviewed live-device demo

Final video: `demo/walkcache-live-demo.mp4`

Duration: **59.733 seconds**, 1920×1080, 30 fps, 1792 decoded frames.

SHA-256: `d834bda3b156137df2693d11297855180e8e5c681fc870417fa05a0647aec945`

Full FFmpeg decode: **passed with no output/errors**.

This is an edit of actual continuous Android emulator and headed Maestro Chromium recordings.
It is not a static-evidence slideshow. Android uses `walkcache_api_34` / API 34 and sequential adb
screenrecord clips at the platform limit. Chromium is the exact headed Maestro browser's CDP
screencast because macOS AVFoundation/`screencapture` permission was blocked; it is not claimed as
desktop capture. Fixture credentials are deterministic test values only.

Every playback factor is visible on its segment and below. Time compression is an edit, not a
wall-clock speedup claim. Repair is limited to one attempt and `MainActivity.java` plus `ids.xml`;
within each lifecycle, its saved YAML and exact `Order #WC-1001 · Mochi sticker pack` assertion stay
byte-identical. The normal selector source is restored and the final Android replay is green. Browser
warm replay uses the exact same identity/YAML with 0 model calls. The WALK-014 fallback remains unchanged.

The fresh Android cold/warm pair and bounded-repair pair are explicitly separate lifecycles. The fresh
cold model authored YAML `e95532ce04a4e125d3bd56e1b9d5d367b31dd67eed4441c56271ca52f3c3d0a1`. The repair fixture is a
different, byte-preserved model-authored candidate from raw run
`walk020-android-20261122T200956-97974`, independently revalidated with production
`remember checkout` before repair; its YAML is `2b863bb62021ec93370ada0a626c90f8b6ee07a8a380c545589f81237060e76f`.
No cold-flow assertion was deleted and no repair classifier was broadened to make these flows interchangeable.

## Final-cut to raw mapping

| Final cut | Label | Raw range | Playback | Raw source |
| --- | --- | --- | --- | --- |
| 0.000–1.500s | LIVE DEVICE DEMO | — | — | title/close only |
| 1.500–13.467s | BEFORE CACHE | 0.000–99.033s | 8.25× | `cold` clips in `demo/generated/android/walk020-android-checkout-06/` |
| 13.467–20.433s | AFTER CACHE | 0.000–26.987s | 3.86× | `warm` clips in `demo/generated/android/walk020-android-checkout-06/` |
| 20.433–27.400s | BEFORE REPAIR | 0.000–41.697s | 5.96× | `stale-repair` clips in `demo/generated/android/walk020-android-checkout-06/` |
| 27.400–36.367s | AFTER REPAIR | 41.697–68.126s | 2.94× | `stale-repair` clips in `demo/generated/android/walk020-android-checkout-06/` |
| 36.367–42.333s | GREEN REPLAY | 0.000–27.041s | 4.51× | `replay` clips in `demo/generated/android/walk020-android-checkout-06/` |
| 42.333–50.200s | CHROMIUM · BEFORE CACHE | 0.000–19.733s | 2.50× | `demo/generated/browser/walk020-20261122T195607-94392/remember.mp4` |
| 50.200–57.733s | CHROMIUM · AFTER CACHE | 0.000–18.900s | 2.50× | `demo/generated/browser/walk020-20261122T195607-94392/run.mp4` |
| 57.733–59.733s | VERIFIED LIVE EVIDENCE | — | — | title/close only |

Detailed per-clip ranges, SHA-256 values, receipt/report/YAML links, and frame counts:
[`live-cut-map.json`](live-evidence/live-cut-map.json). Raw uncut files are intentionally ignored in:
- `demo/generated/android/walk020-android-checkout-06/`
- `demo/generated/browser/walk020-20261122T195607-94392/`

Curated commands/reports/reviews:
- [`Android evidence`](live-evidence/android/)
- [`Chromium evidence`](live-evidence/chromium/)

No external social post or official submission is part of this demo-production ticket.
