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
  blank frames, extracts all eight scene midpoints, and writes `two-rehearsals.json` plus
  `contact-sheet.png`. The frame checks are automated; the contact sheet is retained for human visual
  review. This model run does not claim visual inspection it could not perform.
- Durations are complete captured-process values from the named evidence, not normalized cross-commit
  timing comparisons. The fallback is rendered from evidence rather than represented as a new live
  exploration or a simulator recording.

See [`two-rehearsals.json`](two-rehearsals.json) for the actual final checks and timestamps. Run:

```sh
python3 demo/generate_video.py
python3 demo/rehearse.py
```
