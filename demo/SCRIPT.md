# WalkCache — 58-second evidence-backed demo

**Format:** 1920×1080 MP4, 30 fps, 58 seconds, silent with full on-screen narration. There are no
spoken or implied claims beyond the captions below. This deterministic fallback uses excerpts from
committed real runs, not a fake live run or an edited assertion. Generated `.walkcache/` state,
model credentials, and simulator/model availability are not required to render it.

| Time | Sequence | On-screen/evidence-backed claim |
| --- | --- | --- |
| 00:00–00:05 | WalkCache / Explore. Verify. Cache. Replay. | Learn a QA walk once; replay without a model; repair only bounded staleness. Android + verified Chromium. |
| 00:05–00:13 | Cold exploration | Captured CLI `walk008-cold`: cache miss; 13 model round trips; 11 assertions; pass in 95.499 s; YAML persisted after independent verification. |
| 00:13–00:20 | Saved YAML | An indented excerpt from the reviewed `repaired-checkout.yaml` uses ordinary Maestro selectors and meaningful catalog/cart/order assertions. No WalkCache call is embedded in the YAML. |
| 00:20–00:28 | Warm replay | Captured README runtime review of a later different committed walk: exact cache hit; 0 model calls; 12 assertions; pass in 29.023 s. The warm panel visualizes exact goal/app/build/flow lookup and explicitly distinguishes the earlier 11-assertion cold/repair walks. No same-walk or instantaneous-replay claim. |
| 00:28–00:38 | Stale selector → bounded repair | Captured CLI `walk008-stale-repair`: one failing checkout ID tap; exact Java/XML patch; 1 repair; 0 model calls; 11 assertions passed after rebuild/verification in 65.865 s; proposed diff only, never auto-committed. |
| 00:38–00:45 | Product regression guard | Captured CLI `walk008-product-regression`: meaningful order assertion failed; `product_assertion`; 0 repair attempts; 0 model calls; exit 1. A failure is never repaired into a pass. |
| 00:45–00:51 | Verified Chromium boundary | WALK-010 warm Chromium: cache hit; 12 assertions; 0 model calls in 42.634 s. Web cold exploration and Web repair are explicitly unsupported. Maestro Web is Beta/Experimental. |
| 00:51–00:58 | Architecture and honest close | Exact goal/app/platform/schema/build/flow plus Web adapter/browser identity. Verify before persistence. Android cold/warm/repair and Chromium remember/warm/regression are evidenced; iOS/general repair/instant replay are not claimed. Apache-2.0 repository link. |

Durations describe the complete captured processes identified in their evidence files. They are not a
controlled cross-commit speedup comparison. WALK-005 found historical cold command medians of
96.5–97.0 s versus 24.3–25.9 s warm, but nested Maestro runner ranges overlapped; this video shows
model avoidance and correctness gates rather than claiming universal faster execution.
The assertion count differs because the evidence comes from different committed fixture revisions:
the WALK-008 cold/repair captures have 11 assertions, while the later WALK-012 reviewed walk has 12.

## Raw sources

- `spikes/001-android-walkcache/evidence/walk008-cold-miss.transcript.txt`
- `spikes/001-android-walkcache/evidence/repaired-checkout.yaml`
- `spikes/001-android-walkcache/evidence/walk012-readme-review.txt`
- `spikes/001-android-walkcache/evidence/walk008-stale-repair.transcript.txt`
- `spikes/001-android-walkcache/evidence/walk008-product-regression.transcript.txt`
- `spikes/002-web-walkcache/evidence/walk010-warm.txt`
- `spikes/002-web-walkcache/evidence/walk010-cold-unsupported.txt`
- `docs/ARCHITECTURE.md`
- `LICENSE`

## Render

```sh
python3 demo/generate_video.py
ffmpeg -v error -i demo/walkcache-demo.mp4 -f null -
python3 demo/rehearse.py
```

`generate_video.py` uses Pillow; FFmpeg performs encoding. Generated slide PNGs stay under ignored
`demo/assets/`; the final MP4, script, claim checks and rehearsal reviews are curated release assets.
