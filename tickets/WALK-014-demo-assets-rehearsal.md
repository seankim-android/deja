---
id: WALK-014
title: Prepare deterministic demo assets and rehearse twice
milestone: M7C
priority: P0
status: done
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
- [x] Review every frame for credentials, personal data, tokens, absolute private paths, and secrets.
- [x] Rehearse end to end twice, timing each run and checking every spoken claim against evidence.
- [x] Record any failed take and correct the script or product before final capture.

## Acceptance evidence

Link the script, fallback, raw artifacts, two rehearsal timings, and review checklists. The gate is
complete only when both rehearsals stay at or below 60 seconds and all claims are supported.

Evidence and remaining gate:

- [Script and raw-source claims](../demo/SCRIPT.md), [reproducible renderer](../demo/generate_video.py),
  [58-second MP4](../demo/walkcache-demo.mp4), and [review harness](../demo/rehearse.py).
- [Rejected take and review notes](../demo/rehearsal/REVIEW.md): first image-concat take was rejected
  at 65 seconds / 1,950 frames. Explicit scene trims corrected it without changing product evidence.
- [Two passing rehearsals](../demo/rehearsal/two-rehearsals.json): final video 58.000 seconds,
  1,740 frames decoded in each run, no blank frames, all six claim groups matched committed evidence;
  final review wall times 12.996 s and 10.701 s, playback duration 58 s in both. Corrected SHA-256:
  `3e6c9ec2f4040ec1a8e8bb1e4f8eee2bbe977d6e3f2a69e5b6503cf2f178e461`.
- [Eight-scene contact sheet](../demo/rehearsal/contact-sheet.png) and full-resolution corrected
  encoded scenes passed AI visual inspection with no blocking issues. Every one of the 1,740 encoded
  frames also passed comparison to its reviewed source scene (minimum recorded SSIM > 0.9993).
  Automated source scans, two full decode/pixel checks and caption evidence checks passed. This is
  accurately identified as automated/AI review, not a claim of human sign-off. The first visual pass
  rejected flattened YAML indentation and ambiguous fixture-count wording; the renderer was corrected
  and the encoded scenes were re-inspected before delivery. See [review record](../demo/rehearsal/REVIEW.md).
- Delivery: corrected video uploaded to the authorized Slack thread as file `F0CKVLY06F7` on
  2026-11-22, replacing superseded review upload `F0CKUL64YBD`. Upload is not an external social post
  or hackathon submission. WALK-013 is complete; WALK-015 external/publication gates
  also remain open under WALK-015; they are not demo-production acceptance steps for this ticket.

