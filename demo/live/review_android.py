#!/usr/bin/env python3
"""Validate/curate real Android cold, warm, bounded-repair, and replay takes."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
from typing import Any, Iterator

from PIL import Image, ImageDraw, ImageFont

REPO = Path(__file__).resolve().parents[2]
SPIKE = REPO / "spikes" / "001-android-walkcache"
RAW_ROOT = REPO / "demo" / "generated" / "android"
CURATED = REPO / "demo" / "live-evidence" / "android"
FALLBACK_SHA256 = "3e6c9ec2f4040ec1a8e8bb1e4f8eee2bbe977d6e3f2a69e5b6503cf2f178e461"
VIDEO_PHASES = ["cold", "warm", "stale-repair", "replay"]
PHASES = ["cold", "warm", "repair-baseline", "repair-warm", "stale-repair", "replay"]
SELECTOR_FILES = [
    "app/src/main/java/dev/walkcache/checkout/MainActivity.java",
    "app/src/main/res/values/ids.xml",
]


def load(path: Path) -> Any:
    return json.loads(path.read_text(encoding="utf-8"))


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def nested_values(value: Any, key: str) -> Iterator[Any]:
    if isinstance(value, dict):
        if key in value:
            yield value[key]
        for child in value.values():
            yield from nested_values(child, key)
    elif isinstance(value, list):
        for child in value:
            yield from nested_values(child, key)


def command(command: list[str], cwd: Path = SPIKE) -> str:
    return subprocess.run(
        command, cwd=cwd, check=True, text=True,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
    ).stdout


def ffprobe(path: Path) -> dict[str, Any]:
    return json.loads(command([
        "ffprobe", "-v", "error", "-count_frames", "-show_entries",
        "format=duration,size:stream=codec_name,width,height,nb_read_frames",
        "-of", "json", str(path),
    ], cwd=REPO))


def scan(paths: list[Path]) -> None:
    patterns = {
        "private absolute path": re.compile(r"/(?:Users|home|private/var/folders)/"),
        "common API-token prefix": re.compile(r"(?:sk-[A-Za-z0-9_-]{12,}|AKIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9]{20,})"),
        "authorization material": re.compile(r"(?:Authorization\s*:|Bearer\s+[A-Za-z0-9._-]{12,})", re.I),
    }
    for path in paths:
        text = path.read_text(encoding="utf-8")
        for label, pattern in patterns.items():
            require(not pattern.search(text), f"{label} in {path.name}")


def extract(video: Path, seconds: float, destination: Path) -> None:
    subprocess.run([
        "ffmpeg", "-hide_banner", "-loglevel", "error", "-ss", f"{max(0, seconds):.3f}",
        "-i", str(video), "-frames:v", "1", "-y", str(destination),
    ], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-id", required=True)
    args = parser.parse_args()
    raw = RAW_ROOT / args.run_id
    require(raw.is_dir(), f"Missing Android raw run {args.run_id}")
    CURATED.mkdir(parents=True, exist_ok=True)
    review_dir = REPO / "demo" / "generated" / "review" / args.run_id
    review_dir.mkdir(parents=True, exist_ok=True)

    receipts: dict[str, Any] = {}
    sessions: dict[str, Any] = {}
    captures: dict[str, Any] = {}
    video_info: dict[str, list[dict[str, Any]]] = {}
    curated_text: list[Path] = []

    for phase in PHASES:
        receipt = load(raw / f"{phase}.receipt.json")
        session = load(raw / f"{phase}.session.json")
        capture = load(raw / f"{phase}-capture.json")
        receipts[phase], sessions[phase], captures[phase] = receipt, session, capture
        require(session["cliExitCode"] == 0, f"{phase} CLI failed")
        require(session["avd"] == "walkcache_api_34" and session["device"] == "emulator-5554",
                f"Wrong Android resource in {phase}")
        require(capture["clipCount"] >= 1, f"No continuous emulator video in {phase}")
        infos: list[dict[str, Any]] = []
        for clip in capture["clips"]:
            video = REPO / clip["localFile"]
            require(video.is_file() and video.stat().st_size > 1000, f"Missing raw clip {video.name}")
            require(digest(video) == clip["sha256"], f"Raw clip hash mismatch {video.name}")
            probe = ffprobe(video)
            video_stream = next(stream for stream in probe["streams"] if stream.get("codec_name") == "h264")
            frames = int(video_stream["nb_read_frames"])
            duration = float(probe["format"]["duration"])
            require(frames >= 20 and duration >= 2, f"Invalid/zero-duration Android clip {video.name}")
            decode = subprocess.run(
                ["ffmpeg", "-v", "error", "-i", str(video), "-f", "null", "-"],
                text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            )
            (raw / f"{phase}-clip-{clip['index']:03d}-decode.log").write_text(decode.stdout)
            require(decode.returncode == 0, f"Full decode failed for {video.name}")
            infos.append({
                "index": clip["index"], "file": clip["localFile"], "sha256": clip["sha256"],
                "durationSeconds": duration, "decodedFrames": frames,
                "decodeExitCode": decode.returncode,
                "duplicateDtsWarnings": decode.stdout.count("non monotonically increasing dts"),
                "probe": probe,
            })
        video_info[phase] = infos

        shutil.copyfile(raw / f"{phase}.receipt.json", CURATED / f"{phase}-receipt.json")
        shutil.copyfile(raw / f"{phase}.session.json", CURATED / f"{phase}-session.json")
        shutil.copyfile(raw / f"{phase}-capture.json", CURATED / f"{phase}-capture.json")
        shutil.copyfile(raw / f"{phase}-saved-checkout.yaml", CURATED / f"{phase}-saved-checkout.yaml")
        report = SPIKE / receipt["report"]["reportPath"]
        require(report.is_file(), f"Missing {phase} CLI run report")
        shutil.copyfile(report, CURATED / f"{phase}-run-report.json")
        curated_text.extend([
            CURATED / f"{phase}-receipt.json", CURATED / f"{phase}-session.json",
            CURATED / f"{phase}-capture.json", CURATED / f"{phase}-saved-checkout.yaml",
            CURATED / f"{phase}-run-report.json",
        ])

    cold, warm, repair, replay = (receipts[phase] for phase in VIDEO_PHASES)
    baseline = receipts["repair-baseline"]
    repair_warm = receipts["repair-warm"]
    require(cold["command"] == "explore" and cold["passed"], "Real cold explore did not pass")
    require(cold["modelCalls"] and cold["modelCalls"] > 0, "Cold run has no real model activity")
    require(any((value or 0) > 0 for value in nested_values(cold, "mcpCalls")),
            "Cold run has no real Maestro MCP exploration")
    exploration = cold["exploration"]
    require(exploration["passed"] is True and exploration["verificationDurationMs"] > 0,
            "Cold candidate lacked an independent verification lifecycle")
    verification_artifacts = [
        artifact for artifact in cold["artifacts"]
        if artifact["kind"] == "output" and artifact["path"].endswith("/verification/output.log")
    ]
    require(len(verification_artifacts) == 1, "Cold independent verification log is missing")
    verification_path = SPIKE / verification_artifacts[0]["path"]
    verification_text = verification_path.read_text(encoding="utf-8")
    final_assertion_lines = {
        'Assert that "Order #WC-1001 · Mochi sticker pack" is visible... COMPLETED',
        'Assert that "Order #WC-1001 · Mochi sticker pack", id: screen_confirmation is visible... COMPLETED',
        'Assert that "Order #WC-1001 · Mochi sticker pack", id: dev.walkcache.checkout:id/screen_confirmation is visible... COMPLETED',
    }
    require('Launch app "dev.walkcache.checkout" with clear state... COMPLETED' in verification_text and
            any(line in verification_text for line in final_assertion_lines),
            "Cold independent verification did not pass from fresh clear state through the final assertion")
    stored_memory = load(raw / "cold-stored-memory.json")
    require(stored_memory["origin"] == "explore" and stored_memory["lastValidation"]["passed"] is True and
            stored_memory["flowSha256"] == cold["yamlSha256"] and
            stored_memory["goalKey"] == cold["goalKey"],
            "Cold verified flow was not saved in the local atomic store")
    shutil.copyfile(verification_path, CURATED / "cold-independent-verification.log")
    shutil.copyfile(raw / "cold-stored-memory.json", CURATED / "cold-stored-memory.json")
    curated_text.extend([CURATED / "cold-independent-verification.log", CURATED / "cold-stored-memory.json"])

    require(warm["passed"] and warm["cacheStatus"] == "hit" and warm["modelCalls"] == 0,
            "Warm run is not a passing zero-model exact cache hit")
    require(warm["repairCount"] == 0, "Unexpected warm repair")
    require(cold["identity"] == warm["identity"], "Cold/warm cache identity differs")
    require(cold["yamlSha256"] == warm["yamlSha256"], "Cold/warm YAML differs")
    require(cold["assertions"] == warm["assertions"], "Cold/warm assertion counts differ")

    require(baseline["command"] == "remember" and baseline["passed"] and baseline["modelCalls"] == 0,
            "Separate repair fixture was not independently verified by remember")
    require(repair_warm["passed"] and repair_warm["cacheStatus"] == "hit" and
            repair_warm["modelCalls"] == 0 and repair_warm["repairCount"] == 0,
            "Repair-fixture warm run is not a passing zero-model exact hit")
    require(baseline["identity"] == repair_warm["identity"] and
            baseline["yamlSha256"] == repair_warm["yamlSha256"] and
            baseline["assertions"] == repair_warm["assertions"],
            "Repair fixture baseline/warm identities or YAML differ")
    provenance = load(raw / "repair-fixture-provenance.json")
    require(provenance["candidateBytesCopiedExactly"] is True and
            provenance["separateFromFreshColdWarmLifecycle"] is True and
            provenance["sourceYamlSha256"] == baseline["yamlSha256"] and
            provenance["sourceModelCalls"] > 0,
            "Repair fixture is not traceable to its distinct model-authored cold candidate")
    require((raw / "repair-fixture-candidate.yaml").read_bytes() ==
            (CURATED / "repair-baseline-saved-checkout.yaml").read_bytes(),
            "Remember changed the repair-fixture candidate bytes")
    baseline_memory = load(raw / "repair-baseline-stored-memory.json")
    require(baseline_memory["origin"] == "remember" and
            baseline_memory["lastValidation"]["passed"] is True and
            baseline_memory["flowSha256"] == baseline["yamlSha256"],
            "Independent repair baseline was not atomically stored")
    baseline_outputs = [artifact for artifact in baseline["artifacts"]
                        if artifact["kind"] == "output" and artifact["path"].endswith("/output.log")]
    require(len(baseline_outputs) >= 1, "Repair baseline standalone verification log is missing")
    baseline_output = SPIKE / baseline_outputs[0]["path"]
    baseline_text = baseline_output.read_text(encoding="utf-8")
    final_assertion_passed = (
        'Assert that "Order #WC-1001 · Mochi sticker pack" is visible... COMPLETED' in baseline_text or
        'Assert that "Order #WC-1001 · Mochi sticker pack", id: dev.walkcache.checkout:id/screen_confirmation is visible... COMPLETED' in baseline_text
    )
    require('Launch app "dev.walkcache.checkout" with clear state... COMPLETED' in baseline_text and
            final_assertion_passed,
            "Repair baseline verification did not pass through the unchanged final assertion")
    shutil.copyfile(baseline_output, CURATED / "repair-baseline-verification.log")
    shutil.copyfile(raw / "repair-baseline-stored-memory.json", CURATED / "repair-baseline-stored-memory.json")
    shutil.copyfile(raw / "repair-fixture-provenance.json", CURATED / "repair-fixture-provenance.json")
    shutil.copyfile(raw / "repair-fixture-candidate.yaml", CURATED / "reviewed-repair-fixture.yaml")
    curated_text.extend([
        CURATED / "repair-baseline-verification.log", CURATED / "repair-baseline-stored-memory.json",
        CURATED / "repair-fixture-provenance.json", CURATED / "reviewed-repair-fixture.yaml",
    ])

    require(repair["passed"] and repair["outcome"] == "repaired" and repair["cacheStatus"] == "miss",
            "Stale source run did not complete the expected bounded repair")
    require(repair["modelCalls"] == 0 and repair["repairCount"] == 1,
            "Repair must use one bounded attempt and no model")
    require(repair["yamlSha256"] == baseline["yamlSha256"], "Repair changed its baseline YAML/assertions")
    require(repair["assertions"] == baseline["assertions"], "Repair changed baseline assertion count")
    require(repair["identity"]["buildFingerprint"] != repair_warm["identity"]["buildFingerprint"],
            "Repair did not recompute the changed APK identity")
    require(repair["failure"]["category"] == "selector" and repair["failure"]["repairEligible"] is True,
            "Failure was not the configured repairable Checkout selector")
    details = repair["repair"]["details"]
    require(details["oneAttemptOnly"] is True and details["autoCommit"] is False and
            details["pullRequest"] is False and details["timeBudgetMs"] == 240000,
            "Configured repair bounds changed")
    require(details["diagnosis"]["passed"] is False and
            details["diagnosis"]["failureClassification"]["category"] == "selector",
            "Stale Checkout selector did not fail before repair")
    require(details["build"]["passed"] is True and details["verification"]["passed"] is True and
            details["buildIdentityRecomputed"] is True,
            "Repair did not rebuild/recompute identity/independently verify")
    require(details["persistence"]["atomicWrite"] is True and
            details["preservation"]["assertionsUnchangedByteForByte"] is True,
            "Repair did not preserve assertions and atomically replace verified memory")
    require(len(repair["diffs"]) == 1 and repair["diffs"][0]["affectedFiles"] == SELECTOR_FILES and
            repair["diffs"][0]["autoCommit"] is False, "Repair touched files outside its two-file policy")

    require(replay["passed"] and replay["cacheStatus"] == "hit" and replay["modelCalls"] == 0 and
            replay["repairCount"] == 0, "Final green replay is not an exact zero-model hit")
    # The stale-run receipt retains the input stale-build identity. The verified
    # repaired cache is keyed by the recomputed post-build fingerprint instead.
    require(replay["identity"]["buildFingerprint"] == details["buildFingerprintAfter"] and
            replay["identity"]["buildFingerprint"] != repair["identity"]["buildFingerprint"] and
            {key: value for key, value in replay["identity"].items() if key != "buildFingerprint"} ==
            {key: value for key, value in repair["identity"].items() if key != "buildFingerprint"},
            "Final replay identity differs from the recomputed repaired cache")
    require(replay["yamlSha256"] == baseline["yamlSha256"] and
            replay["assertions"] == baseline["assertions"],
            "Final replay changed repair-fixture YAML/assertions")

    cold_yaml_bytes = (CURATED / "cold-saved-checkout.yaml").read_bytes()
    require((CURATED / "warm-saved-checkout.yaml").read_bytes() == cold_yaml_bytes,
            "Fresh cold/warm saved YAML bytes differ")
    repair_yaml_bytes = (CURATED / "repair-baseline-saved-checkout.yaml").read_bytes()
    for phase in ["repair-warm", "stale-repair", "replay"]:
        require((CURATED / f"{phase}-saved-checkout.yaml").read_bytes() == repair_yaml_bytes,
                f"{phase} repair-fixture YAML bytes differ")
    for yaml_bytes, lifecycle in [(cold_yaml_bytes, "fresh cold/warm"), (repair_yaml_bytes, "repair")]:
        require(b"Order #WC-1001 \xc2\xb7 Mochi sticker pack" in yaml_bytes,
                f"Exact final product assertion is missing from {lifecycle} YAML")
    shutil.copyfile(CURATED / "cold-saved-checkout.yaml", CURATED / "reviewed-model-authored-checkout.yaml")
    curated_text.append(CURATED / "reviewed-model-authored-checkout.yaml")

    source_baseline = load(raw / "baseline-selector-hashes.json")
    current = {path: digest(SPIKE / path) for path in SELECTOR_FILES}
    require(current == source_baseline, "Normal selector source bytes were not restored")
    source_diff = subprocess.run(
        ["git", "diff", "--", *[f"spikes/001-android-walkcache/{path}" for path in SELECTOR_FILES]],
        cwd=REPO, text=True, stdout=subprocess.PIPE, check=True,
    ).stdout
    require(source_diff == "", "Tracked selector files still differ after repair")
    (CURATED / "selector-restoration.diff").write_text(source_diff)
    curated_text.append(CURATED / "selector-restoration.diff")

    selector_setting = command(["adb", "-s", "emulator-5554", "shell", "settings", "get", "global", "walkcache_checkout_selector_stale"]).strip()
    regression_setting = command(["adb", "-s", "emulator-5554", "shell", "settings", "get", "global", "walkcache_product_regression"]).strip()
    avd = command(["adb", "-s", "emulator-5554", "emu", "avd", "name"]).splitlines()[0].strip()
    device_state = command(["adb", "-s", "emulator-5554", "get-state"]).strip()
    require(selector_setting == "null" and regression_setting == "null" and
            avd == "walkcache_api_34" and device_state == "device", "Android fixture/device not restored stable")
    restoration = {
        "avd": avd, "device": "emulator-5554", "api": 34, "state": device_state,
        "selectorSetting": selector_setting, "regressionSetting": regression_setting,
        "selectorSourceHashes": current, "selectorDiffBytes": 0,
        "normalSelectorRestored": True, "finalReplayPassed": True,
        "repairedBuildFingerprint": details["buildFingerprintAfter"],
    }
    (CURATED / "restoration.json").write_text(json.dumps(restoration, indent=2) + "\n")
    curated_text.append(CURATED / "restoration.json")

    repair_artifacts = repair["artifacts"]
    for artifact in repair_artifacts:
        if artifact["kind"] == "diff":
            source = SPIKE / artifact["path"]
            shutil.copyfile(source, CURATED / "bounded-repair.diff")
            curated_text.append(CURATED / "bounded-repair.diff")
        if artifact["kind"] == "output" and artifact["path"].endswith("/diagnosis/output.log"):
            diagnosis = (SPIKE / artifact["path"]).read_text(encoding="utf-8")
            # Keep the complete failure/assertion evidence but never publish the
            # local macOS debug directory. The ignored raw log is not modified.
            diagnosis = re.sub(
                r"(?:/(?:Users|home|private/var/folders)/)[^\s]+",
                "[private local Maestro debug path redacted]", diagnosis,
            )
            (CURATED / "stale-selector-diagnosis.log").write_text(diagnosis, encoding="utf-8")
            curated_text.append(CURATED / "stale-selector-diagnosis.log")
        if artifact["kind"] == "output" and artifact["path"].endswith("/verification/output.log"):
            shutil.copyfile(SPIKE / artifact["path"], CURATED / "repair-verification.log")
            curated_text.append(CURATED / "repair-verification.log")
    require((CURATED / "bounded-repair.diff").is_file() and
            (CURATED / "stale-selector-diagnosis.log").is_file() and
            (CURATED / "repair-verification.log").is_file(), "Missing bounded repair artifacts")

    capture_started = datetime.fromisoformat(captures["stale-repair"]["startedAt"])
    cli_started = datetime.fromisoformat(repair["startedAt"].replace("Z", "+00:00"))
    cli_offset = (cli_started - capture_started).total_seconds()
    diagnosis_end = cli_offset + details["diagnosis"]["durationMs"] / 1000
    total_raw = sum(item["durationSeconds"] for item in video_info["stale-repair"])
    require(1 < diagnosis_end < total_raw - 1, "Diagnosis timestamp does not map inside repair video")

    commands = f"""WALK-020 Android live capture commands
Only walkcache_api_34 / emulator-5554 is used. Run from spike unless noted.

. ./scripts/env.sh
emulator -avd walkcache_api_34 -no-boot-anim
adb -s emulator-5554 wait-for-device
sh scripts/fixture-mode.sh stable
./gradlew :app:assembleDebug
adb -s emulator-5554 install -r -g app/build/outputs/apk/debug/app-debug.apk

# Repository root. Fresh cold/warm are one lifecycle. The separate repair fixture
# is a byte-preserved model-authored candidate independently remembered next.
python3 demo/live/record_android.py --phase cold --goal checkout --run-id {args.run_id}
python3 demo/live/record_android.py --phase warm --run-id {args.run_id}
python3 demo/live/record_android.py --phase repair-baseline --run-id {args.run_id} \
  --repair-flow demo/generated/android/walk020-android-20261122T200956-97974/cold-saved-checkout.yaml
python3 demo/live/record_android.py --phase repair-warm --run-id {args.run_id}
python3 demo/live/record_android.py --phase stale-repair --run-id {args.run_id}
python3 demo/live/record_android.py --phase replay --run-id {args.run_id}
python3 demo/live/review_android.py --run-id {args.run_id}

The repair baseline transparently revalidates a separate, hash-preserved model-
authored candidate; its YAML hash is intentionally distinct from the fresh cold/
warm lifecycle. The stale-repair driver alone performs the exact experiment-only
reversal of evidence/walk007-repair.diff, builds/installs it, and runs production
`run`. The configured repairer restores only MainActivity.java/ids.xml, rebuilds
and independently verifies. Baseline bytes are restored automatically on failure.
No repair is committed. Raw uncut screenrecord clips remain ignored under:
demo/generated/android/{args.run_id}/
"""
    (CURATED / "commands.txt").write_text(commands)
    curated_text.append(CURATED / "commands.txt")

    # Contact sheet from real raw video; images are review artifacts only.
    samples: list[tuple[str, int, float, str]] = []
    for phase, fractions in [
        ("cold", [0.05, 0.35, 0.65, 0.995]), ("warm", [0.97]),
        ("stale-repair", []), ("replay", [0.97]),
    ]:
        total = sum(item["durationSeconds"] for item in video_info[phase])
        for fraction in fractions:
            samples.append((phase, fraction, total * fraction, ""))
    samples.insert(5, ("stale-repair", 0.0, max(0, diagnosis_end - 1), "failure"))
    samples.insert(6, ("stale-repair", 0.0, max(0, total_raw - 1), "verified"))
    cells: list[Image.Image] = []
    font = ImageFont.truetype("/System/Library/Fonts/SFNS.ttf", 17)
    for index, (phase, _, target_seconds, label) in enumerate(samples):
        remaining = target_seconds
        chosen = video_info[phase][-1]
        for info in video_info[phase]:
            if remaining <= info["durationSeconds"]:
                chosen = info
                break
            remaining -= info["durationSeconds"]
        frame_path = review_dir / f"{index + 1:02d}-{phase}.png"
        extract(REPO / chosen["file"], remaining, frame_path)
        image = Image.open(frame_path).convert("RGB")
        image.thumbnail((280, 610))
        cell = Image.new("RGB", (320, 670), "#101419")
        cell.paste(image, ((320 - image.width) // 2, 44))
        draw = ImageDraw.Draw(cell)
        caption = f"{phase.upper()} · raw {target_seconds:.1f}s"
        draw.text((10, 12), caption, font=font, fill="#ecf2ef")
        cells.append(cell)
    sheet = Image.new("RGB", (1280, 1340), "#101419")
    for index, cell in enumerate(cells):
        sheet.paste(cell, ((index % 4) * 320, (index // 4) * 670))
    sheet.save(CURATED / "contact-sheet.png")

    evidence = {
        "schemaVersion": 1,
        "ticket": "WALK-020",
        "runId": args.run_id,
        "validatedAt": datetime.now(timezone.utc).isoformat(),
        "resource": {"avd": "walkcache_api_34", "device": "emulator-5554", "api": 34},
        "phases": {
            phase: {
                key: receipts[phase].get(key) for key in [
                    "command", "outcome", "passed", "cacheStatus", "modelCalls", "modelCallBasis",
                    "durationMs", "assertions", "repairCount", "yamlSha256",
                ]
            } for phase in PHASES
        },
        "rawUncut": video_info,
        "independentColdVerification": True,
        "atomicColdPersistence": True,
        "coldWarmIdentityExactMatch": True,
        "repairBaselineWarmIdentityExactMatch": True,
        "freshColdAndRepairFixtureAreSeparateLifecycles": True,
        "repairBaseline": {
            "command": "remember", "passed": True, "modelCalls": 0,
            "yamlSha256": baseline["yamlSha256"], "assertions": baseline["assertions"],
            "sourceRunId": provenance["sourceRunId"],
            "sourceGoal": provenance["sourceGoal"],
            "sourceModelCalls": provenance["sourceModelCalls"],
            "candidateBytesCopiedExactly": True,
        },
        "repairDiagnosisEndRawSeconds": diagnosis_end,
        "repairLifecycle": {
            "oneAttemptOnly": True, "autoCommit": False, "pullRequest": False,
            "timeBudgetMs": details["timeBudgetMs"],
            "affectedFiles": SELECTOR_FILES,
            "diagnosisDurationMs": details["diagnosis"]["durationMs"],
            "buildDurationMs": details["build"]["durationMs"],
            "verificationDurationMs": details["verification"]["durationMs"],
            "assertionsUnchangedByteForByte": True,
            "atomicWrite": True,
            "diagnosisBuildFingerprint": details["diagnosisBuildFingerprint"],
            "repairedBuildFingerprint": details["buildFingerprintAfter"],
        },
        "normalSelectorRestored": True,
        "finalAssertion": "Order #WC-1001 · Mochi sticker pack",
        "fallbackSha256Unchanged": digest(REPO / "demo" / "walkcache-demo.mp4"),
        "visualReview": "Pending separate AI/human inspection of contact-sheet.png and segment frames",
    }
    require(evidence["fallbackSha256Unchanged"] == FALLBACK_SHA256, "WALK-014 fallback changed")
    evidence_path = CURATED / "evidence.json"
    evidence_path.write_text(json.dumps(evidence, indent=2) + "\n")
    curated_text.append(evidence_path)
    scan(curated_text)
    evidence["secretAndPrivatePathScanPassed"] = True
    evidence_path.write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps(evidence, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
