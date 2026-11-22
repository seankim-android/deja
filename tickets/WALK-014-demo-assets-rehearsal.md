---
id: WALK-014
title: Prepare deterministic demo assets and rehearse twice
milestone: M7C
priority: P0
status: in_progress
depends_on: [WALK-005, WALK-007, WALK-008, WALK-013]
---

# WALK-014 — Prepare deterministic demo assets and rehearse twice

## Outcome

Create a reliable, readable demo whose spoken claims match raw evidence, with a fallback that does
not depend on live model or simulator luck.

## Checklist

- [x] Script a ≤60-second sequence: cold exploration, saved YAML, warm zero-model replay, stale
  selector, bounded repair, and honest support/architecture conclusion.
- [x] Capture readable terminal/dashboard output and retain the raw JSON/YAML/diff for every claim.
- [x] Prepare a deterministic fallback recording from reproducible fixture runs.
- [x] Do not state or imply “instant” wall-clock replay unless WALK-005 supports the comparison.
- [x] Show only verified platforms and measured model-call counts; label unknown values.
- [ ] Review every frame for credentials, personal data, tokens, absolute private paths, and secrets.
- [x] Rehearse end to end twice, timing each run and checking every spoken claim against evidence.
- [x] Record any failed take and correct the script or product before final capture.

## Acceptance evidence

Link the script, fallback, raw artifacts, two rehearsal timings, and review checklists. The gate is
complete only when both rehearsals stay at or below 60 seconds and all claims are supported.

Evidence and remaining gate:

- [Script and raw-source claims](../../demo/SCRIPT.md), [reproducible renderer](../../demo/generate_video.py),
  [58-second MP4](../../demo/walkcache-demo.mp4), and [review harness](../../demo/rehearse.py).
- [Rejected take and review notes](../../demo/rehearsal/REVIEW.md): first image-concat take was rejected
  at 65 seconds / 1,950 frames. Explicit scene trims corrected it without changing product evidence.
- [Two passing rehearsals](../../demo/rehearsal/two-rehearsals.json): final video 58.000 seconds,
  1,740 frames decoded in each run, no blank frames, all six claim groups matched committed evidence;
  review wall times 7.988 s and 7.969 s, playback duration 58 s in both. SHA-256:
  `2a56664421cfb723a777d2da4495354736d56745f5c6b8a440d27028180a38d5`.
- [Eight-scene contact sheet](../../demo/rehearsal/contact-sheet.png) is retained for human visual review.
  Automated source scans, all-frame decode/pixel checks, and caption evidence checks passed; this model
  run does not claim human visual inspection. The every-frame visual/credential review remains open.
- Delivery: uploaded to the authorized Slack thread as file `F0CKUL64YBD` on 2026-11-22. Upload is not
  an external social post or hackathon submission. WALK-013 is complete; WALK-015 external/publication gates
  also remain open, so this ticket stays `in_progress`.

