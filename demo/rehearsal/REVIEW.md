# Demo capture and rehearsal review

## Failed take (rejected before delivery)

- **Renderer:** initial FFmpeg image-concat implementation.
- **Observed:** 65.000 seconds / 1,950 frames despite eight scene durations totaling 58 seconds.
- **Cause:** image-concat padding changed scene timing.
- **Disposition:** rejected by `ffprobe`; not posted or submitted. The renderer was corrected to use
  eight explicit `trim=duration=…`, 30 fps, PTS-reset inputs plus a 58-second output bound.
- No product assertion, cached flow, source repair, or measured result was changed to address this
  recording-only failure.
- A subsequent rehearsal-harness check initially parsed zero frame records because FFmpeg prefixes
  signal-statistics keys with logger metadata. The parser was corrected and both rehearsals rerun;
  this did not alter the 58-second video.

## Final deterministic fallback

- 1920×1080 H.264 MP4, 30 fps, 58.000 seconds / 1,740 frames; no audio track.
- All narration is rendered on-screen. The script contains no fixture passwords, provider keys,
  personal data, bearer tokens, home paths, or raw machine-local artifacts.
- Each numeric claim and outcome maps to literal needles in the committed transcript files listed by
  `rehearse.py`. Warm model calls are zero; cold round trips are identified as 13. Web cold/repair and
  iOS/general repair are not implied to work. No “instant” claim or universal speedup is made.
- `rehearse.py` performs two complete decodes, one signal-statistics check per encoded frame, rejects
  blank frames, extracts all eight scene midpoints, and compares **every one of the 1,740 encoded
  frames** to its scene's reviewed source image. Every encoded-frame SSIM score is at least 0.95;
  actual minimums are recorded in `two-rehearsals.json`. It writes that JSON plus `contact-sheet.png`.
- AI visual inspection covered the contact sheet and full-resolution scenes, then rechecked the
  corrected encoded scenes. No credentials, personal data, tokens or private absolute paths were
  found; the public repository URL and synthetic fixture IDs are intentional. This is not represented
  as human sign-off. The first inspection found Scene 3's YAML indentation was flattened by text
  wrapping. The renderer now preserves semantic indentation; its subtitle, warm-walk labels,
  regression wording, contrast and layout were corrected before final delivery. Final visual verdict:
  **PASS, no blocking issues**. Source hyphens are ASCII; their monospace glyph merely looks wide.
- Durations are complete captured-process values from the named evidence, not normalized cross-commit
  timing comparisons. The fallback is rendered from evidence rather than represented as a new live
  exploration or a simulator recording.

See [`two-rehearsals.json`](two-rehearsals.json) for the actual final checks and timestamps. Run:

```sh
python3 demo/generate_video.py
python3 demo/rehearse.py
```
