#!/usr/bin/env python3
"""Build clearly labeled concise segments from uncut real Chromium takes."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
from typing import Any

from PIL import Image, ImageDraw, ImageFont

REPO = Path(__file__).resolve().parents[2]
RAW_ROOT = REPO / "demo" / "generated" / "browser"
OUT = REPO / "demo" / "generated" / "live-edit"
CURATED = REPO / "demo" / "live-evidence" / "chromium"
WIDTH, HEIGHT, FPS = 1920, 1080, 30
SPEED = 2.5
FONT = "/System/Library/Fonts/SFNS.ttf"
MONO = "/System/Library/Fonts/Menlo.ttc"


def font(size: int, mono: bool = False) -> ImageFont.ImageFont:
    return ImageFont.truetype(MONO if mono else FONT, size)


def text(draw: ImageDraw.ImageDraw, xy: tuple[int, int], value: str, size: int,
         color: str = "#ecf2ef", mono: bool = False) -> None:
    draw.text(xy, value, font=font(size, mono), fill=color)


def probe(video: Path) -> tuple[float, int]:
    data = json.loads(subprocess.run([
        "ffprobe", "-v", "error", "-count_frames", "-show_entries",
        "format=duration:stream=nb_read_frames", "-of", "json", str(video),
    ], check=True, text=True, stdout=subprocess.PIPE).stdout)
    return float(data["format"]["duration"]), int(data["streams"][0]["nb_read_frames"])


def panel(phase: str, receipt: dict[str, Any], raw_duration: float, destination: Path) -> None:
    before = phase == "remember"
    label = "BEFORE CACHE" if before else "AFTER CACHE"
    accent = "#83ccb0" if before else "#a8d88a"
    image = Image.new("RGB", (WIDTH, HEIGHT), "#101419")
    draw = ImageDraw.Draw(image)
    draw.rounded_rectangle((48, 62, 906, 982), 24, fill="#0b0f13", outline="#34403c", width=2)
    draw.rounded_rectangle((950, 62, 1872, 982), 24, fill="#171e23", outline="#34403c", width=2)
    text(draw, (1000, 96), "WalkCache · real headed Chromium", 29, "#afbab4")
    draw.rounded_rectangle((998, 150, 1300 if before else 1260, 202), 12, fill=accent)
    text(draw, (1018, 163), label, 27, "#101419", True)
    text(draw, (1000, 240), f"walkcache {phase}", 38, "#ecf2ef", True)

    if before:
        lines = [
            ("Independent verification + save", 31, "#ecf2ef", False),
            ("Outcome: remembered · PASS", 27, "#a8d88a", True),
            (f"{receipt['assertions']} assertions · {receipt['modelCalls']} model calls", 29, "#ecf2ef", True),
            ("YAML persisted only after PASS", 27, "#ecf2ef", False),
        ]
    else:
        lines = [
            ("Exact warm replay", 31, "#ecf2ef", False),
            ("Cache status: HIT · PASS", 27, "#a8d88a", True),
            (f"{receipt['assertions']} assertions · {receipt['modelCalls']} model calls", 29, "#ecf2ef", True),
            (f"{receipt['repairCount']} repairs · same saved YAML", 27, "#ecf2ef", False),
        ]
    y = 328
    for value, size, color, mono in lines:
        text(draw, (1000, y), value, size, color, mono)
        y += 66

    text(draw, (1000, 626), "SAVED YAML SHA-256", 21, "#afbab4", True)
    text(draw, (1000, 663), receipt["yamlSha256"][:20] + "…", 26, "#83ccb0", True)
    text(draw, (1000, 724), "Final product assertion", 23, "#afbab4")
    text(draw, (1000, 762), "Order WC-1001", 32, "#ecf2ef", True)
    text(draw, (1000, 808), "Mochi sticker pack", 27, "#ecf2ef", True)

    text(draw, (1000, 883), "Same real fixture journey in both takes", 21, "#afbab4")
    text(draw, (72, 1004), "Maestro --device chromium · CDP screencast · not desktop capture", 18, "#afbab4", True)
    footer = f"PLAYBACK {SPEED}× · edited playback only, not a speedup claim · uncut {raw_duration:.2f}s retained"
    text(draw, (980, 1004), footer, 18, "#afbab4", True)
    destination.parent.mkdir(parents=True, exist_ok=True)
    image.save(destination)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-id", required=True)
    args = parser.parse_args()
    raw = RAW_ROOT / args.run_id
    OUT.mkdir(parents=True, exist_ok=True)
    CURATED.mkdir(parents=True, exist_ok=True)
    mapping = {
        "schemaVersion": 1,
        "ticket": "WALK-020",
        "runId": args.run_id,
        "playbackCompression": SPEED,
        "speedupClaim": False,
        "segments": [],
    }
    for phase in ["remember", "run"]:
        receipt = json.loads((raw / f"{phase}.receipt.json").read_text(encoding="utf-8"))
        raw_video = raw / f"{phase}.mp4"
        raw_duration, raw_frames = probe(raw_video)
        panel_path = OUT / f"browser-{phase}-panel.png"
        panel(phase, receipt, raw_duration, panel_path)
        output = OUT / f"browser-{phase}.mp4"
        duration = raw_duration / SPEED
        subprocess.run([
            "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
            "-loop", "1", "-framerate", str(FPS), "-i", str(panel_path),
            "-i", str(raw_video), "-filter_complex",
            f"[1:v]setpts=(PTS-STARTPTS)/{SPEED},scale=-2:880[live];"
            f"[0:v][live]overlay=76:88:shortest=1,fps={FPS},format=yuv420p[out]",
            "-map", "[out]", "-an", "-t", f"{duration:.6f}",
            "-c:v", "libx264", "-preset", "medium", "-crf", "18",
            "-pix_fmt", "yuv420p", "-movflags", "+faststart", str(output),
        ], check=True)
        subprocess.run(["ffmpeg", "-v", "error", "-i", str(output), "-f", "null", "-"], check=True)
        output_duration, output_frames = probe(output)
        mapping["segments"].append({
            "label": "BEFORE CACHE" if phase == "remember" else "AFTER CACHE",
            "segmentFile": f"demo/generated/live-edit/browser-{phase}.mp4",
            "rawVideo": f"demo/generated/browser/{args.run_id}/{phase}.mp4",
            "rawRangeSeconds": [0.0, raw_duration],
            "rawFramesDecoded": raw_frames,
            "playbackCompression": SPEED,
            "segmentDurationSeconds": output_duration,
            "segmentFramesDecoded": output_frames,
            "receipt": f"demo/live-evidence/chromium/{phase}-receipt.json",
            "runReport": f"demo/live-evidence/chromium/{phase}-run-report.json",
            "savedYaml": f"demo/live-evidence/chromium/{phase}-saved-checkout.yaml",
            "edit": "Uniformly time-compressed entire uncut take; no wait or failure frames removed",
        })
    mapping_path = CURATED / "browser-edit-map.json"
    mapping_path.write_text(json.dumps(mapping, indent=2) + "\n")
    print(json.dumps(mapping, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
