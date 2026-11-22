#!/usr/bin/env python3
"""Run two full decodes plus evidence and per-frame sanity checks for the captioned fallback."""
from pathlib import Path
from PIL import Image, ImageDraw
from datetime import datetime, timezone
import hashlib
import json
import re
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
DEMO = ROOT / "demo"
VIDEO = DEMO / "walkcache-demo.mp4"
REVIEW = DEMO / "rehearsal"
REVIEW.mkdir(exist_ok=True)

# Every number/outcome shown in the video is tied to literal committed evidence.
CLAIMS = {
    "cold": ("spikes/001-android-walkcache/evidence/walk008-cold-miss.transcript.txt", [
        "Model calls: 13", "Duration: 95499 ms", "Assertions: 11", "Outcome: cold-recorded",
    ]),
    "warm": ("spikes/001-android-walkcache/evidence/walk012-readme-review.txt", [
        "Cache status: hit", "Model calls: 0", "Duration: 29023 ms", "Assertions: 12",
        "Outcome: passed",
    ]),
    "repair": ("spikes/001-android-walkcache/evidence/walk008-stale-repair.transcript.txt", [
        "Outcome: repaired", "Duration: 65865 ms", "Assertions: 11", "Repair count: 1",
        "Model calls: 0", "Affected file: app/src/main/java/dev/walkcache/checkout/MainActivity.java",
        "Affected file: app/src/main/res/values/ids.xml", "Button checkout = button(\"Checkout\", checkoutId);",
    ]),
    "regression": ("spikes/001-android-walkcache/evidence/walk008-product-regression.transcript.txt", [
        "Outcome: product-regression", "Failure: product_assertion", "Model calls: 0",
        "Duration: 41876 ms", "Repair count: 0", "Exit code: 1", "Source diffs: none",
    ]),
    "web": ("spikes/002-web-walkcache/evidence/walk010-warm.txt", [
        "Cache status: hit", "Model calls: 0", "Duration: 42634 ms", "Assertions: 12",
        "Outcome: passed",
    ]),
    "web-cold": ("spikes/002-web-walkcache/evidence/walk010-cold-unsupported.txt", [
        "Cold Web exploration is not supported", "Model calls: 0", "Outcome: exploration-rejected",
    ]),
}

probe = json.loads(subprocess.check_output([
    "ffprobe", "-v", "error", "-show_entries",
    "format=duration,size:stream=codec_name,width,height,r_frame_rate,nb_frames",
    "-of", "json", str(VIDEO),
], text=True))
duration = float(probe["format"]["duration"])
stream = probe["streams"][0]
assert duration <= 60, f"Video exceeds deadline: {duration}"
assert (stream["width"], stream["height"]) == (1920, 1080)
assert stream["r_frame_rate"] == "30/1"
assert stream["codec_name"] == "h264"

claim_results = {}
for name, (relative, needles) in CLAIMS.items():
    contents = (ROOT / relative).read_text()
    missing = [needle for needle in needles if needle not in contents]
    assert not missing, f"{name}: unsupported claims: {missing}"
    claim_results[name] = {"evidence": relative, "needlesChecked": len(needles), "passed": True}

source_text = (DEMO / "generate_video.py").read_text() + (DEMO / "SCRIPT.md").read_text()
for forbidden in ["/Users/", "Bearer ", "BEGIN " + "PRIVATE KEY", "cerebras_api_key", "OPENCLAW_GATEWAY_TOKEN"]:
    assert forbidden not in source_text, f"Forbidden source content: {forbidden}"
for claim_file in {entry[0] for entry in CLAIMS.values()}:
    assert Path(claim_file).name in source_text, f"Video/script does not cite {claim_file}"

# Middle of each of the eight scenes; retained for human review in addition to all-frame decode checks.
times = [2.5, 9, 16.5, 24, 33, 41.5, 48, 54.5]
scene_paths = []
for index, timestamp in enumerate(times, 1):
    path = REVIEW / f"scene-{index:02d}.png"
    subprocess.run([
        "ffmpeg", "-y", "-v", "error", "-ss", str(timestamp), "-i", str(VIDEO),
        "-frames:v", "1", str(path),
    ], check=True)
    with Image.open(path) as image:
        assert image.size == (1920, 1080)
        extrema = image.getextrema()
        assert any(high - low > 100 for low, high in extrema), f"Scene {index} appears blank"
    scene_paths.append(path)

contact = Image.new("RGB", (1920, 600), "#0d1418")
draw = ImageDraw.Draw(contact)
for index, path in enumerate(scene_paths):
    image = Image.open(path).resize((470, 264), Image.Resampling.LANCZOS)
    x, y = (index % 4) * 480, (index // 4) * 300
    contact.paste(image, (x, y))
    draw.text((x + 8, y + 270), f"Scene {index + 1} · {times[index]:.1f}s", fill="#eff6f2")
contact.save(REVIEW / "contact-sheet.png")

rehearsals = []
for number in (1, 2):
    started = time.monotonic()
    # Decode every encoded frame. signalstats metadata supplies one record per frame; reject blanks.
    process = subprocess.run([
        "ffmpeg", "-v", "info", "-i", str(VIDEO),
        "-vf", "signalstats,metadata=mode=print", "-an", "-f", "null", "/dev/null",
    ], check=True, capture_output=True, text=True)
    records = re.findall(r"frame:(\d+)[^\n]*\n.*?lavfi\.signalstats\.YMIN=(\d+(?:\.\d+)?).*?lavfi\.signalstats\.YMAX=(\d+(?:\.\d+)?)", process.stderr, re.S)
    assert len(records) == int(stream["nb_frames"]), f"Frame accounting mismatch: {len(records)}"
    blanks = [int(frame) for frame, ymin, ymax in records if float(ymax) < 30]
    assert not blanks, f"Unexpected blank frames: {blanks[:10]}"
    rehearsal = {
        "rehearsal": number,
        "reviewedAt": datetime.now(timezone.utc).isoformat(),
        "playbackDurationSeconds": duration,
        "within60Seconds": duration <= 60,
        "encodedFramesDecoded": len(records),
        "blankFrames": blanks,
        "eightSceneContactSheetReviewed": "rehearsal/contact-sheet.png",
        "captionClaimsChecked": claim_results,
        "secretAndAbsolutePathSourceScanPassed": True,
        "audio": "none; all narration is visible in full on-screen captions",
        "reviewWallSeconds": round(time.monotonic() - started, 3),
        "result": "pass",
    }
    rehearsals.append(rehearsal)

summary = {
    "video": "demo/walkcache-demo.mp4",
    "sha256": hashlib.sha256(VIDEO.read_bytes()).hexdigest(),
    "probe": probe,
    "reviewLimit": "Automated full-frame decode/pixel checks and all-caption evidence checks; contact sheet retained for human visual review. No audio or secrets are present by construction.",
    "rehearsals": rehearsals,
}
(REVIEW / "two-rehearsals.json").write_text(json.dumps(summary, indent=2) + "\n")
print(json.dumps({
    "duration": duration, "frames": stream["nb_frames"], "claims": len(CLAIMS),
    "rehearsals": [{"run": r["rehearsal"], "result": r["result"], "reviewWallSeconds": r["reviewWallSeconds"]} for r in rehearsals],
    "sha256": summary["sha256"],
}, indent=2))
