# WALK-020 final live-demo review

## Definitive deliverable

- Video: `demo/walkcache-live-demo.mp4`
- SHA-256: `d834bda3b156137df2693d11297855180e8e5c681fc870417fa05a0647aec945`
- Duration: **59.733333 seconds**; 1920×1080, constant 30 fps, H.264, 1,792 fully decoded frames.
- Complete FFmpeg decode of the final encoded MP4: **passed with no warnings or errors**.
- Every evidence segment maps to a raw time range, playback factor, raw SHA-256, receipt, run report,
  and saved YAML in `live-cut-map.json`. The two short title/close cards are identified as generated
  cards and are not presented as product evidence.
- WALK-014 fallback SHA-256 remains
  `3e6c9ec2f4040ec1a8e8bb1e4f8eee2bbe977d6e3f2a69e5b6503cf2f178e461`.

## Independent AI visual review

A vision-capable `view_image` review inspected `final-contact-sheet.png` and terminal frames extracted
from the actual encoded MP4, not just the intermediate segments. It confirmed:

- real Android launcher/login/cart/confirmation UI and real rendered Chromium fixture UI;
- visible `Order #WC-1001 · Mochi sticker pack` at cold, warm, after-repair and green-replay ends;
- visible `Order WC-1001 · Mochi sticker pack` at both Chromium ends (the web fixture intentionally uses
  no `#`, while Android does);
- BEFORE/AFTER CACHE, BEFORE/AFTER REPAIR and GREEN REPLAY badges, assertion/model-call counts, distinct
  YAML prefixes, and playback-compression disclosures;
- `checkout_button ID missing` and separate repair-fixture disclosure on the failing cart frame, with no
  confirmation shown before repair;
- no blank/black evidence frames, private paths, secrets, clipping, misleading desktop-capture claim,
  or wall-clock speedup claim.

The initial review's presentation advisories were corrected without changing source evidence. Cold and
before-repair now say `EXPECTED FINAL ASSERTION`, successful terminal segments say `FINAL PRODUCT
ASSERTION`, Chromium contact-sheet labels are prefixed `CHROMIUM`, and the close says `No repair was
committed to VCS`. A second final-frame/contact-sheet review returned **PASS, no blockers**. Smaller
in-device text remains legible at 1080p and is repeated in large report-panel type where appropriate.

This is automated and independent AI visual review. **Human sign-off is not claimed.**

## Evidence scope

The fresh Android cold/warm YAML (`e95532…`) and separate repair-fixture YAML (`2b863b…`) are not
misrepresented as one identity chain. Their distinct provenance and receipts are documented in
[android/REVIEW.md](android/REVIEW.md). The repair was one deterministic, selector-only attempt affecting
only `MainActivity.java` and `ids.xml`; no failed assertion was edited and no repair was committed to VCS.
The normal selector source was restored and final replay passed on the recomputed repaired-build identity.

Chromium is the exact headed Maestro browser's CDP screencast, with held latest browser frames during
static waits. macOS desktop capture permission was blocked; this video does not claim desktop capture.
Browser playback is uniformly 2.5× and Android playback factors are printed on every panel. These are
edit disclosures, not performance claims.

Raw recordings and review frames remain ignored under `demo/generated/`. Curated text evidence passes
private-path, common-token and authorization-material scans. Final repeat verification is available with:

```sh
python3 demo/live/check_live_demo.py --check-raw
```

No external social post or official hackathon submission is part of this demo-production ticket.
