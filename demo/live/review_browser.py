#!/usr/bin/env python3
"""Validate and curate a paired headed-Chromium remember/warm-run capture."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
from typing import Any

from PIL import Image, ImageDraw, ImageFont

REPO = Path(__file__).resolve().parents[2]
SPIKE = REPO / "spikes" / "002-web-walkcache"
RAW_ROOT = REPO / "demo" / "generated" / "browser"
CURATED = REPO / "demo" / "live-evidence" / "chromium"
FALLBACK_SHA256 = "3e6c9ec2f4040ec1a8e8bb1e4f8eee2bbe977d6e3f2a69e5b6503cf2f178e461"


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def command_json(command: list[str]) -> Any:
    return json.loads(subprocess.run(command, check=True, text=True, stdout=subprocess.PIPE).stdout)


def ffprobe(path: Path) -> dict[str, Any]:
    return command_json([
        "ffprobe", "-v", "error", "-count_frames", "-show_entries",
        "format=duration,size:stream=codec_name,width,height,r_frame_rate,nb_frames,nb_read_frames",
        "-of", "json", str(path),
    ])


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def scan_secret_free(paths: list[Path]) -> None:
    patterns = {
        "private absolute path": re.compile(r"/(?:Users|home|private/var/folders)/"),
        "common API-token prefix": re.compile(r"(?:sk-[A-Za-z0-9_-]{12,}|AKIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9]{20,})"),
        "authorization material": re.compile(r"(?:Authorization\s*:|Bearer\s+[A-Za-z0-9._-]{12,})", re.I),
    }
    for path in paths:
        text = path.read_text(encoding="utf-8")
        for label, pattern in patterns.items():
            require(not pattern.search(text), f"{label} found in curated file {path.name}")


def extract_frame(video: Path, seconds: float, destination: Path) -> None:
    subprocess.run([
        "ffmpeg", "-hide_banner", "-loglevel", "error", "-ss", f"{seconds:.3f}",
        "-i", str(video), "-frames:v", "1", "-y", str(destination),
    ], check=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-id", required=True)
    args = parser.parse_args()
    raw = RAW_ROOT / args.run_id
    require(raw.is_dir(), f"Missing raw browser pair {args.run_id}")
    CURATED.mkdir(parents=True, exist_ok=True)
    review_frames = REPO / "demo" / "generated" / "review" / args.run_id
    review_frames.mkdir(parents=True, exist_ok=True)

    receipts: dict[str, Any] = {}
    sessions: dict[str, Any] = {}
    captures: dict[str, Any] = {}
    probes: dict[str, Any] = {}
    evidence_files: list[Path] = []
    durations: dict[str, float] = {}

    for phase in ["remember", "run"]:
        label = "BEFORE CACHE" if phase == "remember" else "AFTER CACHE"
        receipt_path = raw / f"{phase}.receipt.json"
        session_path = raw / f"{phase}.session.json"
        # The rejected first automated take predates per-phase session names;
        # accepted takes always use phase.session.json. This fallback only
        # accommodates the first successful remember before that naming fix.
        if phase == "remember" and not session_path.exists():
            legacy = raw / "session.json"
            if legacy.exists():
                legacy.rename(session_path)
        capture_path = raw / f"{phase}.screencast.json"
        video = raw / f"{phase}.mp4"
        for required in [receipt_path, session_path, capture_path, video]:
            require(required.exists(), f"Missing {phase} evidence: {required.name}")

        receipt = load(receipt_path)
        session = load(session_path)
        capture = load(capture_path)
        receipts[phase], sessions[phase], captures[phase] = receipt, session, capture
        require(session["label"] == label, f"Wrong {phase} label")
        require(session["cliExitCode"] == 0 and session["recorderExitCode"] == 0,
                f"{phase} recording processes failed")
        require(capture["label"] == label, f"Wrong {phase} screencast label")
        require(capture["captureScope"].startswith("real Chromium CDP screencast"),
                "Capture scope must disclose browser-only recording")
        require(capture["screencastEventCount"] >= 8, f"Too few real browser changes in {phase}")
        require(capture["emittedFrameCount"] >= 300, f"Incomplete continuous video in {phase}")

        probe = ffprobe(video)
        probes[phase] = probe
        stream = probe["streams"][0]
        duration = float(probe["format"]["duration"])
        durations[phase] = duration
        require(stream["codec_name"] == "h264", f"Unexpected {phase} codec")
        require(int(stream.get("nb_read_frames", stream["nb_frames"])) >= 300,
                f"ffprobe decoded too few {phase} frames")
        require(duration >= 10, f"Raw {phase} video is too short to contain the real journey")
        subprocess.run(["ffmpeg", "-v", "error", "-i", str(video), "-f", "null", "-"], check=True)

        shutil.copyfile(receipt_path, CURATED / f"{phase}-receipt.json")
        shutil.copyfile(session_path, CURATED / f"{phase}-session.json")
        # Curate the summary without raw per-frame payloads; raw frame receipts
        # stay next to the uncut video in ignored generated storage.
        capture_summary = {key: value for key, value in capture.items() if key != "frames"}
        (CURATED / f"{phase}-capture.json").write_text(json.dumps(capture_summary, indent=2) + "\n")
        (CURATED / f"{phase}-ffprobe.json").write_text(json.dumps(probe, indent=2) + "\n")
        shutil.copyfile(raw / f"{phase}-saved-checkout.yaml", CURATED / f"{phase}-saved-checkout.yaml")
        report_path = SPIKE / receipt["report"]["reportPath"]
        require(report_path.is_file(), f"Missing real CLI report for {phase}")
        shutil.copyfile(report_path, CURATED / f"{phase}-run-report.json")
        evidence_files.extend([
            CURATED / f"{phase}-receipt.json", CURATED / f"{phase}-session.json",
            CURATED / f"{phase}-capture.json", CURATED / f"{phase}-saved-checkout.yaml",
            CURATED / f"{phase}-run-report.json",
        ])

    before, after = receipts["remember"], receipts["run"]
    require(before["command"] == "remember" and before["outcome"] == "remembered" and before["passed"],
            "remember was not independently verified and persisted")
    require(before["cacheStatus"] == "not-checked" and before["modelCalls"] == 0,
            "remember does not prove independent verification with zero model calls")
    require(after["command"] == "run" and after["outcome"] == "passed" and after["passed"],
            "warm replay did not pass")
    require(after["cacheStatus"] == "hit" and after["modelCalls"] == 0,
            "warm replay is not an exact zero-model cache hit")
    require(after["repairCount"] == 0, "Unexpected Chromium repair")
    require(before["assertions"] == after["assertions"] == 12, "Unexpected assertion count")
    require(before["identity"] == after["identity"], "remember/run cache identity differs")
    require(before["yamlSha256"] == after["yamlSha256"], "remember/run YAML hash differs")
    candidate = SPIKE / "evidence" / "checkout.yaml"
    require(digest(candidate) == before["yamlSha256"], "Saved YAML differs from reviewed candidate")
    saved = (CURATED / "remember-saved-checkout.yaml").read_text(encoding="utf-8")
    require('assertVisible: "Order WC-1001 · Mochi sticker pack"' in saved,
            "Meaningful final order assertion changed")
    shutil.copyfile(candidate, CURATED / "reviewed-checkout.yaml")
    evidence_files.append(CURATED / "reviewed-checkout.yaml")

    require(digest(REPO / "demo" / "walkcache-demo.mp4") == FALLBACK_SHA256,
            "The completed WALK-014 fallback changed")
    commands = f"""WALK-020 Chromium live capture commands
All paths below are relative to the repository unless noted. Fixture server: loopback only.

# Terminal 1
cd spikes/002-web-walkcache
bun server.ts

# Terminal 2, repository root; real headed Maestro --device chromium
python3 demo/live/record_chromium.py --phase remember --run-id {args.run_id}
python3 demo/live/record_chromium.py --phase run --run-id {args.run_id}
python3 demo/live/review_browser.py --run-id {args.run_id}

Raw uncut recordings (ignored):
demo/generated/browser/{args.run_id}/remember.mp4
demo/generated/browser/{args.run_id}/run.mp4

Capture method:
Real Chromium Page.startScreencast frames from the exact Maestro browser process,
held during static waits at 30 fps. This is not desktop/window capture; direct
AVFoundation and screencapture probes were blocked by macOS screen permissions.
"""
    (CURATED / "commands.txt").write_text(commands)
    evidence_files.append(CURATED / "commands.txt")

    permission = """WALK-020 macOS capture permission probe

ffmpeg -hide_banner -f avfoundation -list_devices true -i ''
Result: AVFoundation listed no video devices and returned Input/output error.

ffmpeg -f avfoundation -i '0:none' -t 0.5 out.mp4
ffmpeg -f avfoundation -i '1:none' -t 0.5 out.mp4
Result: both returned Invalid device index / Input/output error.

screencapture -x screen.png
Result: could not create image from display.

Consequence: no desktop/window-video claim is made. Browser evidence is the real
Chromium CDP screencast from the same headed Maestro browser process instead.
"""
    (CURATED / "capture-permission.txt").write_text(permission)
    evidence_files.append(CURATED / "capture-permission.txt")

    # Six frames from the real takes support visual review; they are not used
    # to synthesize any video segment.
    selections = [
        ("remember", fraction, label) for fraction, label in [
            (0.02, "opening"), (0.25, "quarter"), (0.50, "middle"), (0.75, "three-quarter"), (0.97, "final"),
        ]
    ] + [("run", 0.97, "final")]
    sheet_cells: list[Image.Image] = []
    font = ImageFont.truetype("/System/Library/Fonts/SFNS.ttf", 18)
    for index, (phase, fraction, name) in enumerate(selections):
        seconds = max(0.0, min(durations[phase] - 0.05, durations[phase] * fraction))
        frame_path = review_frames / f"{index + 1:02d}-{phase}-{name}.png"
        extract_frame(raw / f"{phase}.mp4", seconds, frame_path)
        image = Image.open(frame_path).convert("RGB")
        image.thumbnail((420, 450))
        cell = Image.new("RGB", (440, 500), "#101419")
        cell.paste(image, ((440 - image.width) // 2, 42))
        draw = ImageDraw.Draw(cell)
        phase_label = "BEFORE CACHE" if phase == "remember" else "AFTER CACHE"
        draw.text((12, 10), f"{phase_label} · raw {seconds:.2f}s", font=font, fill="#ecf2ef")
        sheet_cells.append(cell)
    sheet = Image.new("RGB", (1320, 1000), "#101419")
    for index, cell in enumerate(sheet_cells):
        sheet.paste(cell, ((index % 3) * 440, (index // 3) * 500))
    sheet.save(CURATED / "contact-sheet.png")

    evidence = {
        "schemaVersion": 1,
        "ticket": "WALK-020",
        "runId": args.run_id,
        "validatedAt": datetime.now(timezone.utc).isoformat(),
        "captureScope": "Exact headed Maestro Chromium browser via CDP screencast; not desktop capture",
        "rawUncut": {
            phase: {
                "video": f"demo/generated/browser/{args.run_id}/{phase}.mp4",
                "sha256": digest(raw / f"{phase}.mp4"),
                "durationSeconds": durations[phase],
                "screencastEvents": captures[phase]["screencastEventCount"],
                "emittedFrames": captures[phase]["emittedFrameCount"],
            } for phase in ["remember", "run"]
        },
        "beforeCache": {
            key: before.get(key) for key in [
                "outcome", "passed", "cacheStatus", "modelCalls", "modelCallBasis",
                "durationMs", "assertions", "repairCount", "yamlSha256",
            ]
        },
        "afterCache": {
            key: after.get(key) for key in [
                "outcome", "passed", "cacheStatus", "modelCalls", "modelCallBasis",
                "durationMs", "assertions", "repairCount", "yamlSha256",
            ]
        },
        "identityExactMatch": True,
        "finalAssertion": "Order WC-1001 · Mochi sticker pack",
        "fallbackSha256Unchanged": FALLBACK_SHA256,
        "fullDecodePassed": True,
        "visualReview": "Pending separate AI/human inspection of contact-sheet.png and segment frames",
    }
    evidence_path = CURATED / "evidence.json"
    evidence_path.write_text(json.dumps(evidence, indent=2) + "\n")
    evidence_files.append(evidence_path)
    for path in CURATED.glob("*-ffprobe.json"):
        evidence_files.append(path)
    scan_secret_free(evidence_files)
    evidence["secretAndPrivatePathScanPassed"] = True
    evidence_path.write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps(evidence, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
