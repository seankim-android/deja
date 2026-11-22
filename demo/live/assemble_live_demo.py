#!/usr/bin/env python3
"""Assemble the <=60s reviewed live cut and its complete raw-evidence map."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import re
import subprocess
from typing import Any

from PIL import Image, ImageDraw, ImageFont

REPO = Path(__file__).resolve().parents[2]
EDIT = REPO / "demo" / "generated" / "live-edit"
EVIDENCE = REPO / "demo" / "live-evidence"
FINAL = REPO / "demo" / "walkcache-live-demo.mp4"
FONT = "/System/Library/Fonts/SFNS.ttf"
MONO = "/System/Library/Fonts/Menlo.ttc"
WIDTH, HEIGHT, FPS = 1920, 1080, 30
TITLE_DURATION = 1.5
CLOSE_DURATION = 2.0


def font(size: int, mono: bool = False) -> ImageFont.ImageFont:
    return ImageFont.truetype(MONO if mono else FONT, size)


def text(draw: ImageDraw.ImageDraw, xy: tuple[int, int], value: str, size: int,
         color: str = "#ecf2ef", mono: bool = False) -> None:
    draw.text(xy, value, font=font(size, mono), fill=color)


def probe(path: Path) -> dict[str, Any]:
    return json.loads(subprocess.run([
        "ffprobe", "-v", "error", "-count_frames", "-show_entries",
        "format=duration,size:stream=codec_name,width,height,r_frame_rate,nb_read_frames,codec_type",
        "-of", "json", str(path),
    ], check=True, text=True, stdout=subprocess.PIPE).stdout)


def duration(path: Path) -> float:
    return float(probe(path)["format"]["duration"])


def make_card(kind: str, png: Path, output: Path) -> None:
    image = Image.new("RGB", (WIDTH, HEIGHT), "#101419")
    draw = ImageDraw.Draw(image)
    if kind == "title":
        text(draw, (140, 250), "WalkCache", 92)
        text(draw, (140, 380), "Explore. Verify. Cache. Replay.", 48, "#83ccb0")
        text(draw, (140, 520), "ACTUAL ANDROID + CHROMIUM LIVE TAKES", 28, "#afbab4", True)
        text(draw, (140, 610), "Cold / warm / stale / bounded repair", 33)
        text(draw, (140, 900), "Raw uncut recordings substantiate every edited segment", 25, "#afbab4")
    else:
        text(draw, (140, 280), "Verify first. Replay exactly.", 64)
        text(draw, (140, 420), "Repair only bounded selector staleness.", 42, "#83ccb0")
        text(draw, (140, 570), "No failed assertion was rewritten. No repair was committed to VCS.", 28)
        text(draw, (140, 660), "Playback compression is labeled; no wall-clock speedup claim.", 28, "#afbab4")
        text(draw, (140, 840), "WALK-020 · raw timestamp/report map included", 25, "#afbab4", True)
    png.parent.mkdir(parents=True, exist_ok=True)
    image.save(png)
    seconds = TITLE_DURATION if kind == "title" else CLOSE_DURATION
    subprocess.run([
        "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y", "-loop", "1",
        "-framerate", str(FPS), "-i", str(png), "-t", f"{seconds:.3f}",
        "-an", "-c:v", "libx264", "-preset", "medium", "-crf", "18",
        "-pix_fmt", "yuv420p", "-movflags", "+faststart", str(output),
    ], check=True)


def extract(video: Path, seconds: float, destination: Path) -> None:
    subprocess.run([
        "ffmpeg", "-hide_banner", "-loglevel", "error", "-ss", f"{seconds:.3f}",
        "-i", str(video), "-frames:v", "1", "-y", str(destination),
    ], check=True)


def scan(paths: list[Path]) -> None:
    patterns = [
        re.compile(r"/(?:Users|home|private/var/folders)/"),
        re.compile(r"(?:sk-[A-Za-z0-9_-]{12,}|AKIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9]{20,})"),
        re.compile(r"(?:Authorization\s*:|Bearer\s+[A-Za-z0-9._-]{12,})", re.I),
    ]
    for path in paths:
        value = path.read_text(encoding="utf-8")
        if any(pattern.search(value) for pattern in patterns):
            raise RuntimeError(f"Secret/private-path scan failed for {path.name}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--android-run-id", required=True)
    parser.add_argument("--browser-run-id", required=True)
    args = parser.parse_args()
    android = json.loads((EVIDENCE / "android" / "android-edit-map.json").read_text(encoding="utf-8"))
    android_evidence = json.loads((EVIDENCE / "android" / "evidence.json").read_text(encoding="utf-8"))
    browser = json.loads((EVIDENCE / "chromium" / "browser-edit-map.json").read_text(encoding="utf-8"))
    if android["runId"] != args.android_run_id or browser["runId"] != args.browser_run_id:
        parser.error("Run IDs do not match reviewed edit maps")
    title = EDIT / "live-title.mp4"
    closing = EDIT / "live-close.mp4"
    make_card("title", EDIT / "live-title.png", title)
    make_card("close", EDIT / "live-close.png", closing)

    ordered: list[tuple[str, str, Path, dict[str, Any] | None]] = [
        ("title", "LIVE DEVICE DEMO", title, None),
        *[(entry["name"], entry["label"], REPO / entry["segmentFile"], {"kind": "android", "entry": entry})
          for entry in android["segments"]],
        *[(f"browser-{index + 1}", entry["label"], REPO / entry["segmentFile"], {"kind": "chromium", "entry": entry})
          for index, entry in enumerate(browser["segments"])],
        ("close", "VERIFIED LIVE EVIDENCE", closing, None),
    ]
    expected = sum(TITLE_DURATION if name == "title" else CLOSE_DURATION if name == "close" else duration(path)
                   for name, _, path, _ in ordered)
    if expected > 60:
        raise RuntimeError(f"Refusing demo over 60 seconds: {expected:.3f}")

    concat = []
    for _, _, path, _ in ordered:
        concat.append(f"file '{path}'")
    list_path = EDIT / "live-concat.txt"
    # Absolute paths occur only in this ignored FFmpeg-generated input file;
    # curated commands/maps always use repository-relative paths.
    list_path.write_text("\n".join(concat) + "\n")
    subprocess.run([
        "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
        "-f", "concat", "-safe", "0", "-i", str(list_path),
        "-an", "-c:v", "libx264", "-preset", "medium", "-crf", "18",
        "-pix_fmt", "yuv420p", "-movflags", "+faststart", str(FINAL),
    ], check=True)
    decode = subprocess.run(
        ["ffmpeg", "-v", "error", "-i", str(FINAL), "-f", "null", "-"],
        text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
    )
    if decode.returncode != 0 or decode.stdout:
        raise RuntimeError(f"Final full decode failed/warned: {decode.stdout[:2000]}")
    final_probe = probe(FINAL)
    final_duration = float(final_probe["format"]["duration"])
    video_stream = next(stream for stream in final_probe["streams"] if stream.get("codec_type") == "video")
    if final_duration > 60 or video_stream["codec_name"] != "h264" or \
            (video_stream["width"], video_stream["height"]) != (WIDTH, HEIGHT) or \
            video_stream["r_frame_rate"] != f"{FPS}/1" or int(video_stream["nb_read_frames"]) < 1500:
        raise RuntimeError("Final codec/duration/dimension/frame-rate/frame-count gate failed")

    start = 0.0
    segments = []
    for name, label, path, source in ordered:
        segment_duration = duration(path)
        item: dict[str, Any] = {
            "name": name, "label": label,
            "finalCutRangeSeconds": [round(start, 6), round(start + segment_duration, 6)],
            "durationSeconds": segment_duration,
            "segmentSha256": hashlib.sha256(path.read_bytes()).hexdigest(),
        }
        if source:
            item.update(source["entry"])
            item["kind"] = source["kind"]
            if source["kind"] == "chromium":
                item["label"] = f"CHROMIUM · {source['entry']['label']}"
        else:
            item["source"] = "Short generated title/close card only; no product evidence frame"
        segments.append(item)
        start += segment_duration

    mapping = {
        "schemaVersion": 1,
        "ticket": "WALK-020",
        "generatedAt": datetime.now(timezone.utc).isoformat(),
        "finalVideo": "demo/walkcache-live-demo.mp4",
        "finalSha256": hashlib.sha256(FINAL.read_bytes()).hexdigest(),
        "finalDurationSeconds": final_duration,
        "finalDecodedFrames": int(video_stream["nb_read_frames"]),
        "fps": FPS,
        "dimensions": [WIDTH, HEIGHT],
        "fullDecodePassed": True,
        "noWallClockSpeedupClaim": True,
        "androidRunId": args.android_run_id,
        "browserRunId": args.browser_run_id,
        "segments": segments,
    }
    map_path = EVIDENCE / "live-cut-map.json"
    map_path.write_text(json.dumps(mapping, indent=2) + "\n")
    (EVIDENCE / "final-ffprobe.json").write_text(json.dumps(final_probe, indent=2) + "\n")

    rows = []
    for item in segments:
        raw_range = item.get("rawRangeSeconds")
        raw = f"`{item['rawVideo']}`" if "rawVideo" in item else \
            f"`{item['rawPhase']}` clips in `demo/generated/android/{args.android_run_id}/`" if "rawPhase" in item else "title/close only"
        source_range = f"{raw_range[0]:.3f}–{raw_range[1]:.3f}s" if raw_range else "—"
        compression = f"{item['playbackCompression']:.2f}×" if "playbackCompression" in item else "—"
        rows.append(
            f"| {item['finalCutRangeSeconds'][0]:.3f}–{item['finalCutRangeSeconds'][1]:.3f}s | "
            f"{item['label']} | {source_range} | {compression} | {raw} |"
        )
    script = f"""# WalkCache — reviewed live-device demo

Final video: `demo/walkcache-live-demo.mp4`

Duration: **{final_duration:.3f} seconds**, 1920×1080, 30 fps, {video_stream['nb_read_frames']} decoded frames.

SHA-256: `{mapping['finalSha256']}`

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
cold model authored YAML `{android_evidence['phases']['cold']['yamlSha256']}`. The repair fixture is a
different, byte-preserved model-authored candidate from raw run
`{android_evidence['repairBaseline']['sourceRunId']}`, independently revalidated with production
`remember checkout` before repair; its YAML is `{android_evidence['repairBaseline']['yamlSha256']}`.
No cold-flow assertion was deleted and no repair classifier was broadened to make these flows interchangeable.

## Final-cut to raw mapping

| Final cut | Label | Raw range | Playback | Raw source |
| --- | --- | --- | --- | --- |
{chr(10).join(rows)}

Detailed per-clip ranges, SHA-256 values, receipt/report/YAML links, and frame counts:
[`live-cut-map.json`](live-evidence/live-cut-map.json). Raw uncut files are intentionally ignored in:
- `demo/generated/android/{args.android_run_id}/`
- `demo/generated/browser/{args.browser_run_id}/`

Curated commands/reports/reviews:
- [`Android evidence`](live-evidence/android/)
- [`Chromium evidence`](live-evidence/chromium/)

No external social post or official submission is part of this demo-production ticket.
"""
    (REPO / "demo" / "LIVE_SCRIPT.md").write_text(script)

    # Review frames from the actual final encoded video.
    review_dir = REPO / "demo" / "generated" / "review" / "final-live"
    review_dir.mkdir(parents=True, exist_ok=True)
    cells = []
    font18 = font(18)
    for index, item in enumerate(segments):
        start_s, end_s = item["finalCutRangeSeconds"]
        midpoint = (start_s + end_s) / 2
        frame = review_dir / f"{index + 1:02d}-{item['name']}.png"
        extract(FINAL, midpoint, frame)
        image = Image.open(frame).convert("RGB").resize((480, 270))
        cell = Image.new("RGB", (480, 300), "#101419")
        cell.paste(image, (0, 30))
        ImageDraw.Draw(cell).text((8, 5), item["label"], font=font18, fill="#ecf2ef")
        cells.append(cell)
    rows_count = (len(cells) + 3) // 4
    sheet = Image.new("RGB", (1920, rows_count * 300), "#101419")
    for index, cell in enumerate(cells):
        sheet.paste(cell, ((index % 4) * 480, (index // 4) * 300))
    sheet.save(EVIDENCE / "final-contact-sheet.png")

    validation = {
        "schemaVersion": 1,
        "ticket": "WALK-020",
        "finalSha256": mapping["finalSha256"],
        "durationSeconds": final_duration,
        "decodedFrames": int(video_stream["nb_read_frames"]),
        "ffprobePassed": True,
        "fullDecodePassedWithoutWarnings": True,
        "rawMappingComplete": all("rawRangeSeconds" in item or item["name"] in ["title", "close"]
                                  for item in segments),
        "privatePathAndSecretTextScanPassed": False,
        "visualReview": "Pending separate AI/human review of final-contact-sheet.png and key full-resolution frames",
        "fallbackSha256Unchanged": hashlib.sha256((REPO / "demo" / "walkcache-demo.mp4").read_bytes()).hexdigest(),
    }
    validation_path = EVIDENCE / "final-validation.json"
    validation_path.write_text(json.dumps(validation, indent=2) + "\n")
    scan([map_path, EVIDENCE / "final-ffprobe.json", REPO / "demo" / "LIVE_SCRIPT.md"])
    validation["privatePathAndSecretTextScanPassed"] = True
    validation_path.write_text(json.dumps(validation, indent=2) + "\n")
    print(json.dumps(mapping, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
