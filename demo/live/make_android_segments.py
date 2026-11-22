#!/usr/bin/env python3
"""Render labeled concise Android segments from uncut adb screenrecord clips."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
from typing import Any

from PIL import Image, ImageDraw, ImageFont

REPO = Path(__file__).resolve().parents[2]
RAW_ROOT = REPO / "demo" / "generated" / "android"
OUT = REPO / "demo" / "generated" / "live-edit"
CURATED = REPO / "demo" / "live-evidence" / "android"
WIDTH, HEIGHT, FPS = 1920, 1080, 30
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
        "format=duration:stream=nb_read_frames,codec_name", "-of", "json", str(video),
    ], check=True, text=True, stdout=subprocess.PIPE).stdout)
    stream = next(stream for stream in data["streams"] if stream.get("codec_name") == "h264")
    return float(data["format"]["duration"]), int(stream["nb_read_frames"])


def render_panel(name: str, label: str, title: str, command_name: str,
                 lines: list[tuple[str, int, str, bool]], receipt: dict[str, Any],
                 speed: float, raw_slice: float, raw_total: float, destination: Path) -> None:
    image = Image.new("RGB", (WIDTH, HEIGHT), "#101419")
    draw = ImageDraw.Draw(image)
    draw.rounded_rectangle((48, 62, 906, 982), 24, fill="#0b0f13", outline="#34403c", width=2)
    draw.rounded_rectangle((950, 62, 1872, 982), 24, fill="#171e23", outline="#34403c", width=2)
    text(draw, (72, 88), "REAL ANDROID UI", 20, "#afbab4", True)
    text(draw, (1000, 96), f"WalkCache · {title}", 29, "#afbab4")
    badge_width = max(260, 40 + int(draw.textlength(label, font=font(27, True))))
    draw.rounded_rectangle((998, 150, 998 + badge_width, 202), 12, fill="#83ccb0")
    text(draw, (1018, 163), label, 27, "#101419", True)
    text(draw, (1000, 240), command_name, 38, "#ecf2ef", True)
    y = 328
    for value, size, color, mono in lines:
        text(draw, (1000, y), value, size, color, mono)
        y += 66

    assertion_heading = "EXPECTED FINAL ASSERTION" if name in {"android-cold", "android-before-repair"} else "FINAL PRODUCT ASSERTION"
    text(draw, (1000, 750), assertion_heading, 21, "#afbab4", True)
    text(draw, (1000, 790), "Order #WC-1001", 32, "#ecf2ef", True)
    text(draw, (1000, 836), "Mochi sticker pack", 27, "#ecf2ef", True)
    text(draw, (1000, 899), f"YAML {receipt['yamlSha256'][:20]}…", 22, "#83ccb0", True)
    text(draw, (72, 1004), "walkcache_api_34 · API 34 · continuous adb screenrecord", 22, "#afbab4", True)
    footer = f"PLAYBACK {speed:.1f}× · no speedup claim · raw slice {raw_slice:.2f}s / full take {raw_total:.2f}s"
    text(draw, (1000, 1004), footer, 20, "#afbab4", True)
    destination.parent.mkdir(parents=True, exist_ok=True)
    image.save(destination)


def render_segment(name: str, clips: list[dict[str, Any]], start: float, end: float,
                   target: float, panel_path: Path, output: Path) -> float:
    speed = (end - start) / target
    command = ["ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
               "-loop", "1", "-framerate", str(FPS), "-i", str(panel_path)]
    for info in clips:
        command += ["-i", str(REPO / info["file"])]
    filters = []
    concat_inputs = []
    for index, _ in enumerate(clips, start=1):
        filters.append(f"[{index}:v]fps={FPS},scale=1080:2400,setsar=1,format=yuv420p[v{index}]")
        concat_inputs.append(f"[v{index}]")
    filters.append(f"{''.join(concat_inputs)}concat=n={len(clips)}:v=1:a=0[full]")
    filters.append(f"[full]trim=start={start:.6f}:end={end:.6f},setpts=(PTS-STARTPTS)/{speed:.9f},scale=-2:880[live]")
    filters.append(f"[0:v][live]overlay=272:88:shortest=1,fps={FPS},format=yuv420p[out]")
    command += [
        "-filter_complex", ";".join(filters), "-map", "[out]", "-an", "-t", f"{target:.6f}",
        "-c:v", "libx264", "-preset", "medium", "-crf", "18", "-pix_fmt", "yuv420p",
        "-movflags", "+faststart", str(output),
    ]
    subprocess.run(command, check=True)
    subprocess.run(["ffmpeg", "-v", "error", "-i", str(output), "-f", "null", "-"], check=True)
    return speed


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-id", required=True)
    args = parser.parse_args()
    raw = RAW_ROOT / args.run_id
    evidence = json.loads((CURATED / "evidence.json").read_text(encoding="utf-8"))
    receipts = {
        phase: json.loads((raw / f"{phase}.receipt.json").read_text(encoding="utf-8"))
        for phase in ["cold", "warm", "stale-repair", "replay"]
    }
    captures = {
        phase: json.loads((raw / f"{phase}-capture.json").read_text(encoding="utf-8"))
        for phase in receipts
    }
    OUT.mkdir(parents=True, exist_ok=True)
    diagnosis_end = evidence["repairDiagnosisEndRawSeconds"]
    specs: dict[str, dict[str, Any]] = {
        "android-cold": {
            "phase": "cold", "start": 0.0, "target": 12.0, "label": "BEFORE CACHE",
            "title": "cold exploration", "command": "walkcache explore",
            "lines": [
                ("Model authors a real UI journey", 29, "#ecf2ef", False),
                (f"{receipts['cold']['modelCalls']} model round trips", 29, "#ecf2ef", True),
                ("Independent fresh-state PASS", 27, "#a8d88a", True),
                (f"{receipts['cold']['assertions']} assertions · save after PASS", 25, "#ecf2ef", True),
            ],
        },
        "android-warm": {
            "phase": "warm", "start": 0.0, "target": 7.0, "label": "AFTER CACHE",
            "title": "warm replay", "command": "walkcache run",
            "lines": [
                ("Exact goal/app/build/flow hit", 29, "#ecf2ef", False),
                ("Cache status: HIT · PASS", 27, "#a8d88a", True),
                (f"{receipts['warm']['assertions']} assertions · 0 model calls", 27, "#ecf2ef", True),
                ("0 repairs · same saved YAML", 27, "#ecf2ef", False),
            ],
        },
        "android-before-repair": {
            "phase": "stale-repair", "start": 0.0, "end": diagnosis_end,
            "target": 7.0, "label": "BEFORE REPAIR", "title": "stale selector · separate repair fixture",
            "command": "checkout_button ID missing",
            "lines": [
                ("Checkout tap fails; prior steps pass", 27, "#ecf2ef", False),
                ("Classified: selector", 29, "#ffb7a9", True),
                ("0 model calls · repair eligible", 27, "#ecf2ef", True),
                ("One bounded attempt is allowed", 27, "#ecf2ef", False),
            ],
        },
        "android-after-repair": {
            "phase": "stale-repair", "start": diagnosis_end,
            "target": 9.0, "label": "AFTER REPAIR", "title": "bounded repair · separate fixture",
            "command": "two files · one attempt",
            "lines": [
                ("MainActivity.java + ids.xml only", 27, "#ecf2ef", True),
                ("Rebuild + independent PASS", 27, "#a8d88a", True),
                (f"{receipts['stale-repair']['assertions']} assertions · 0 model calls", 27, "#ecf2ef", True),
                ("Atomic save · YAML unchanged", 27, "#ecf2ef", False),
            ],
        },
        "android-replay": {
            "phase": "replay", "start": 0.0, "target": 6.0, "label": "GREEN REPLAY",
            "title": "repaired-fixture green replay", "command": "walkcache run",
            "lines": [
                ("Normal selector state restored", 29, "#ecf2ef", False),
                ("Exact repaired cache: HIT", 27, "#a8d88a", True),
                (f"{receipts['replay']['assertions']} assertions · 0 model calls", 27, "#ecf2ef", True),
                ("0 repairs · final order PASS", 27, "#ecf2ef", False),
            ],
        },
    }

    mapping: dict[str, Any] = {
        "schemaVersion": 1,
        "ticket": "WALK-020",
        "runId": args.run_id,
        "freshColdWarmAndRepairLifecyclesSeparate": True,
        "speedupClaim": False,
        "segments": [],
    }
    for name, spec in specs.items():
        phase = spec["phase"]
        infos = evidence["rawUncut"][phase]
        clips = []
        offset = 0.0
        for info in infos:
            clips.append({
                "file": info["file"], "sha256": info["sha256"],
                "startSeconds": offset,
                "endSeconds": offset + info["durationSeconds"],
            })
            offset += info["durationSeconds"]
        total = offset
        start = float(spec["start"])
        end = float(spec.get("end", total))
        target = float(spec["target"])
        panel_path = OUT / f"{name}-panel.png"
        speed = (end - start) / target
        render_panel(name, spec["label"], spec["title"], spec["command"], spec["lines"],
                     receipts[phase], speed, end - start, total, panel_path)
        output = OUT / f"{name}.mp4"
        actual_speed = render_segment(name, clips, start, end, target, panel_path, output)
        duration, frames = probe(output)
        entry = {
            "name": name, "label": spec["label"], "segmentFile": f"demo/generated/live-edit/{name}.mp4",
            "rawPhase": phase, "rawRangeSeconds": [start, end], "rawTotalSeconds": total,
            "rawClips": clips, "playbackCompression": actual_speed,
            "segmentDurationSeconds": duration, "segmentFramesDecoded": frames,
            "receipt": f"demo/live-evidence/android/{phase}-receipt.json",
            "runReport": f"demo/live-evidence/android/{phase}-run-report.json",
            "savedYaml": f"demo/live-evidence/android/{phase}-saved-checkout.yaml",
            "edit": "Uniformly time-compressed stated raw range; source frames normalized to 30 fps; no waits/failures selectively removed within range",
        }
        if phase in {"stale-repair", "replay"}:
            entry["baselineReceipt"] = "demo/live-evidence/android/repair-baseline-receipt.json"
            entry["baselineRunReport"] = "demo/live-evidence/android/repair-baseline-run-report.json"
            entry["baselineSavedYaml"] = "demo/live-evidence/android/repair-baseline-saved-checkout.yaml"
            entry["baselineProvenance"] = "demo/live-evidence/android/repair-fixture-provenance.json"
        mapping["segments"].append(entry)
    path = CURATED / "android-edit-map.json"
    path.write_text(json.dumps(mapping, indent=2) + "\n")
    print(json.dumps(mapping, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
