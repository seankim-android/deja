#!/usr/bin/env python3
"""Read-only final gate for the curated WALK-020 live demo.

This checks the actual encoded deliverable and its evidence. It never changes
receipts, assertions, review status, upload status, fixture source, or videos.
Use --check-raw to also rehash/decode the ignored uncut local recordings.
Use --require-upload only after the authorized Slack upload receipt exists.
"""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
from typing import Any

REPO = Path(__file__).resolve().parents[2]
DEMO = REPO / "demo"
EVIDENCE = DEMO / "live-evidence"
FALLBACK_SHA256 = "3e6c9ec2f4040ec1a8e8bb1e4f8eee2bbe977d6e3f2a69e5b6503cf2f178e461"
PHASES = ("cold", "warm", "stale-repair", "replay")


def load(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def probe(path: Path) -> dict[str, Any]:
    return json.loads(subprocess.run([
        "ffprobe", "-v", "error", "-count_frames", "-show_entries",
        "format=duration,size:stream=codec_name,codec_type,width,height,r_frame_rate,nb_read_frames",
        "-of", "json", str(path),
    ], check=True, text=True, stdout=subprocess.PIPE).stdout)


def decode(path: Path, allow_android_dts_warnings: bool = False) -> str:
    result = subprocess.run(
        ["ffmpeg", "-v", "error", "-i", str(path), "-f", "null", "-"],
        text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
    )
    output = result.stdout
    if allow_android_dts_warnings:
        # adb screenrecord emits duplicate DTS values on some static emulator
        # frames. Full decoding still succeeds; concise/final outputs normalize
        # timestamps to 30 fps and are required to decode with zero warnings.
        lines = [line for line in output.splitlines() if
                 "non monotonically increasing dts" not in line and
                 not line.strip().startswith("Last message repeated")]
        output = "\n".join(lines)
    require(result.returncode == 0 and not output,
            f"Full decode failed or emitted warnings for {path.relative_to(REPO)}: {result.stdout[:2000]}")
    return result.stdout


def relative_file(value: str) -> Path:
    path = Path(value)
    require(not path.is_absolute() and ".." not in path.parts, f"Non-relative evidence path: {value}")
    resolved = REPO / path
    require(resolved.is_file(), f"Missing evidence file: {value}")
    return resolved


def scan_text() -> int:
    patterns = {
        "private absolute path": re.compile(r"/(?:Users|home|private/var/folders)/"),
        "common API token": re.compile(r"(?:sk-[A-Za-z0-9_-]{12,}|AKIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9]{20,})"),
        "authorization material": re.compile(r"(?:Authorization\s*:|Bearer\s+[A-Za-z0-9._-]{12,})", re.I),
    }
    paths = [DEMO / "LIVE_SCRIPT.md"]
    paths.extend(path for path in EVIDENCE.rglob("*") if path.is_file() and
                 path.suffix in {".json", ".md", ".txt", ".yaml", ".yml", ".diff", ".log"})
    scanned = 0
    for path in paths:
        value = path.read_text(encoding="utf-8")
        for label, pattern in patterns.items():
            require(not pattern.search(value), f"{label} in {path.relative_to(REPO)}")
        scanned += 1
    return scanned


def review_complete(value: Any) -> bool:
    if isinstance(value, str):
        return "pending" not in value.lower() and bool(value.strip())
    if isinstance(value, dict):
        return value.get("aiVisualReviewPassed") is True
    return False


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check-raw", action="store_true", help="Rehash and fully decode ignored raw clips")
    parser.add_argument("--require-upload", action="store_true", help="Require the authorized Slack receipt")
    args = parser.parse_args()

    final = DEMO / "walkcache-live-demo.mp4"
    require(final.is_file() and final.stat().st_size > 100_000, "Final live MP4 is missing or implausibly small")
    mapping = load(EVIDENCE / "live-cut-map.json")
    validation = load(EVIDENCE / "final-validation.json")
    android = load(EVIDENCE / "android" / "evidence.json")
    chromium = load(EVIDENCE / "chromium" / "evidence.json")
    require(mapping["androidRunId"] == android["runId"], "Android evidence/cut run ID mismatch")
    require(mapping["browserRunId"] == chromium["runId"], "Chromium evidence/cut run ID mismatch")
    require(digest(final) == mapping["finalSha256"] == validation["finalSha256"], "Final MP4 hash mismatch")
    require(mapping["noWallClockSpeedupClaim"] is True, "Wall-clock speedup disclaimer is missing")

    info = probe(final)
    stream = next(item for item in info["streams"] if item.get("codec_type") == "video")
    final_duration = float(info["format"]["duration"])
    frames = int(stream["nb_read_frames"])
    require(final_duration <= 60 and abs(final_duration - mapping["finalDurationSeconds"]) < 0.001,
            "Final duration exceeds 60 seconds or differs from its map")
    require(stream["codec_name"] == "h264" and stream["r_frame_rate"] == "30/1" and
            (stream["width"], stream["height"]) == (1920, 1080) and frames == mapping["finalDecodedFrames"],
            "Final encoding/frame metadata differs from the accepted deliverable")
    decode(final)

    require(validation["ffprobePassed"] is True and validation["fullDecodePassedWithoutWarnings"] is True and
            validation["rawMappingComplete"] is True and
            validation["privatePathAndSecretTextScanPassed"] is True,
            "Recorded final automated validation is incomplete")
    require(review_complete(validation["visualReview"]), "Final AI visual review remains pending")
    require(review_complete(android["visualReview"]), "Android AI visual review remains pending")
    require(review_complete(chromium["visualReview"]), "Chromium AI visual review remains pending")


    cut_position = 0.0
    evidence_segments = 0
    for segment in mapping["segments"]:
        start, end = segment["finalCutRangeSeconds"]
        require(abs(start - cut_position) < 0.001 and end > start,
                f"Non-contiguous final cut at {segment['name']}")
        require(abs(segment["durationSeconds"] - (end - start)) < 0.001,
                f"Segment duration mismatch at {segment['name']}")
        cut_position = end
        if segment["name"] in {"title", "close"}:
            require(segment.get("source", "").startswith("Short generated title/close"),
                    "Title/close source is not accurately disclosed")
            continue
        evidence_segments += 1
        require(len(segment.get("rawRangeSeconds", [])) == 2 and
                segment["rawRangeSeconds"][0] >= 0 and
                segment["rawRangeSeconds"][1] > segment["rawRangeSeconds"][0],
                f"Missing raw timestamp range for {segment['name']}")
        require(segment.get("playbackCompression", 0) > 0 and
                ("compressed" in segment.get("edit", "") or "PLAYBACK" in segment.get("edit", "")),
                f"Playback edit is not disclosed for {segment['name']}")
        for key in ("receipt", "runReport", "savedYaml", "baselineReceipt", "baselineRunReport",
                    "baselineSavedYaml", "baselineProvenance"):
            if key in segment:
                relative_file(segment[key])
        if segment["kind"] == "android":
            require(segment.get("rawClips") and all("sha256" in clip for clip in segment["rawClips"]),
                    f"Android raw clip map incomplete for {segment['name']}")
        else:
            relative_file(segment["rawVideo"]) if args.check_raw else None
        if args.check_raw:
            if segment["kind"] == "android":
                for clip in segment["rawClips"]:
                    raw_path = relative_file(clip["file"])
                    require(digest(raw_path) == clip["sha256"], f"Raw clip hash mismatch: {clip['file']}")
                    decode(raw_path, allow_android_dts_warnings=True)
            else:
                raw_path = relative_file(segment["rawVideo"])
                decode(raw_path)
    require(evidence_segments == 7, "Expected five Android and two Chromium evidence segments")
    require(abs(cut_position - final_duration) < 0.05, "Cut map does not cover the final duration")

    android_map = load(EVIDENCE / "android" / "android-edit-map.json")
    browser_map = load(EVIDENCE / "chromium" / "browser-edit-map.json")
    require(android_map["speedupClaim"] is False and browser_map["speedupClaim"] is False,
            "Segment map contains a speedup claim")
    require(len(android_map["segments"]) == 5 and len(browser_map["segments"]) == 2,
            "Unexpected segment count")

    all_phases = ("cold", "warm", "repair-baseline", "repair-warm", "stale-repair", "replay")
    receipts = {phase: load(EVIDENCE / "android" / f"{phase}-receipt.json") for phase in all_phases}
    cold, warm, repair, replay = (receipts[phase] for phase in PHASES)
    baseline = receipts["repair-baseline"]
    repair_warm = receipts["repair-warm"]
    require(cold["command"] == "explore" and cold["passed"] and cold["modelCalls"] > 0,
            "Cold Android exploration is not real/passing")
    require(warm["passed"] and warm["cacheStatus"] == "hit" and warm["modelCalls"] == 0 and
            warm["identity"] == cold["identity"] and warm["yamlSha256"] == cold["yamlSha256"],
            "Warm Android replay is not an exact zero-model hit")
    require(baseline["command"] == "remember" and baseline["passed"] and baseline["modelCalls"] == 0,
            "Separate Android repair fixture was not independently remembered")
    require(repair_warm["passed"] and repair_warm["cacheStatus"] == "hit" and
            repair_warm["modelCalls"] == 0 and repair_warm["identity"] == baseline["identity"] and
            repair_warm["yamlSha256"] == baseline["yamlSha256"],
            "Repair-fixture warm replay is not an exact zero-model hit")
    require(repair["passed"] and repair["outcome"] == "repaired" and repair["repairCount"] == 1 and
            repair["modelCalls"] == 0 and repair["yamlSha256"] == baseline["yamlSha256"],
            "Android bounded-repair invariant failed")
    details = repair["repair"]["details"]
    require(details["oneAttemptOnly"] is True and details["autoCommit"] is False and
            details["pullRequest"] is False and details["persistence"]["atomicWrite"] is True and
            details["preservation"]["assertionsUnchangedByteForByte"] is True,
            "Android repair bounds or persistence invariant failed")
    require(replay["passed"] and replay["cacheStatus"] == "hit" and replay["modelCalls"] == 0 and
            replay["repairCount"] == 0 and
            replay["identity"]["buildFingerprint"] == details["buildFingerprintAfter"] and
            {key: value for key, value in replay["identity"].items() if key != "buildFingerprint"} ==
            {key: value for key, value in repair["identity"].items() if key != "buildFingerprint"},
            "Final Android green replay/recomputed-build identity invariant failed")
    cold_yaml = (EVIDENCE / "android" / "cold-saved-checkout.yaml").read_bytes()
    repair_yaml = (EVIDENCE / "android" / "repair-baseline-saved-checkout.yaml").read_bytes()
    for yaml_bytes, lifecycle in [(cold_yaml, "fresh cold/warm"), (repair_yaml, "repair")]:
        require(b"Order #WC-1001 \xc2\xb7 Mochi sticker pack" in yaml_bytes,
                f"Final Android product assertion changed or disappeared in {lifecycle}")
    require((EVIDENCE / "android" / "warm-saved-checkout.yaml").read_bytes() == cold_yaml,
            "Fresh cold/warm Android YAML bytes changed")
    for phase in ("repair-warm", "stale-repair", "replay"):
        require((EVIDENCE / "android" / f"{phase}-saved-checkout.yaml").read_bytes() == repair_yaml,
                f"Android repair-fixture YAML bytes changed in {phase}")
    provenance = load(EVIDENCE / "android" / "repair-fixture-provenance.json")
    require(provenance["candidateBytesCopiedExactly"] is True and
            provenance["separateFromFreshColdWarmLifecycle"] is True and
            provenance["sourceYamlSha256"] == baseline["yamlSha256"] and
            provenance["sourceModelCalls"] > 0,
            "Separate repair-fixture provenance is incomplete")
    require((EVIDENCE / "android" / "selector-restoration.diff").read_bytes() == b"",
            "Selector source restoration diff is not empty")
    restoration = load(EVIDENCE / "android" / "restoration.json")
    require(restoration["normalSelectorRestored"] is True and restoration["finalReplayPassed"] is True and
            restoration["selectorSetting"] == "null" and restoration["regressionSetting"] == "null",
            "Android stable fixture restoration was not recorded")

    browser_receipts = {
        phase: load(EVIDENCE / "chromium" / f"{phase}-receipt.json") for phase in ("remember", "run")
    }
    remember, run = browser_receipts["remember"], browser_receipts["run"]
    require(remember["passed"] and run["passed"] and run["cacheStatus"] == "hit" and
            run["modelCalls"] == 0 and run["repairCount"] == 0 and
            remember["identity"] == run["identity"] and remember["yamlSha256"] == run["yamlSha256"],
            "Chromium exact warm replay invariant failed")
    require((EVIDENCE / "chromium" / "remember-saved-checkout.yaml").read_bytes() ==
            (EVIDENCE / "chromium" / "run-saved-checkout.yaml").read_bytes(),
            "Chromium YAML bytes changed")

    require(digest(DEMO / "walkcache-demo.mp4") == FALLBACK_SHA256,
            "Reviewed WALK-014 fallback was modified")
    if args.require_upload:
        upload = load(EVIDENCE / "slack-upload.json")
        require(upload.get("channel") == "C0CKR1JT42Z" and
                upload.get("threadTs") == "1795305727.220879" and upload.get("fileId") and
                upload.get("sha256") == mapping["finalSha256"],
                "Authorized Slack upload receipt is absent or does not match this MP4")

    scanned = scan_text()
    result = {
        "status": "PASS",
        "ticket": "WALK-020",
        "finalSha256": mapping["finalSha256"],
        "durationSeconds": final_duration,
        "decodedFrames": frames,
        "dimensions": [1920, 1080],
        "fps": 30,
        "codec": "h264",
        "evidenceSegments": evidence_segments,
        "privatePathAndSecretTextScanPassed": True,
        "textFilesScanned": scanned,
        "rawHashAndDecodeChecked": args.check_raw,
        "slackUploadReceiptChecked": args.require_upload,
        "fallbackSha256Unchanged": FALLBACK_SHA256,
        "humanSignOffClaimed": False,
    }
    print(json.dumps(result, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
