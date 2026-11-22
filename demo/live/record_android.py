#!/usr/bin/env python3
"""Capture real emulator UI and CLI evidence for the authorized WALK-020 phases.

Only walkcache_api_34 / emulator-5554 and the native WALK-001 fixture/cache are
used. Each phase starts a fresh adb screenrecord sequence; Android's 180-second
limit starts sequential raw clips when necessary. No app assertion or repair
limit is edited, and the stale source probe has byte-level restoration.
"""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
from typing import Any

REPO = Path(__file__).resolve().parents[2]
SPIKE = REPO / "spikes" / "001-android-walkcache"
RAW_ROOT = REPO / "demo" / "generated" / "android"
DEVICE = "emulator-5554"
AVD = "walkcache_api_34"
APP_ID = "dev.walkcache.checkout"
RUN_ID = re.compile(r"^[a-zA-Z0-9][a-zA-Z0-9-]{2,90}$")
SELECTOR_FILES = [
    Path("app/src/main/java/dev/walkcache/checkout/MainActivity.java"),
    Path("app/src/main/res/values/ids.xml"),
]


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def digest_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def digest(path: Path) -> str:
    return digest_bytes(path.read_bytes())


def android_environment() -> dict[str, str]:
    env = os.environ.copy()
    sdk = "/opt/homebrew/share/android-commandlinetools"
    java = "/opt/homebrew/opt/openjdk@21"
    env.update({
        "ANDROID_HOME": sdk,
        "ANDROID_SDK_ROOT": sdk,
        "JAVA_HOME": java,
        "WALKCACHE_DEVICE": DEVICE,
        "MAESTRO_CLI_NO_ANALYTICS": "1",
        "MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED": "true",
    })
    env["PATH"] = f"{Path.home()}/.maestro/bin:{sdk}/emulator:{sdk}/platform-tools:{java}/bin:/opt/homebrew/bin:{env.get('PATH', '')}"
    openclaw = shutil.which("openclaw", path=env["PATH"])
    if openclaw:
        env["WALKCACHE_OPENCLAW"] = openclaw
    return env


def adb(*arguments: str, stdout: Any = subprocess.DEVNULL) -> subprocess.Popen[bytes]:
    return subprocess.Popen(
        ["adb", "-s", DEVICE, *arguments], env=android_environment(),
        stdout=stdout, stderr=subprocess.STDOUT, start_new_session=True,
    )


def checked(command: list[str], cwd: Path = SPIKE, timeout: int = 120) -> str:
    return subprocess.run(
        command, cwd=cwd, env=android_environment(), check=True, timeout=timeout,
        text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
    ).stdout


def logged(command: list[str], log: Path, timeout: int = 240) -> None:
    with log.open("wb") as output:
        process = subprocess.Popen(
            command, cwd=SPIKE, env=android_environment(),
            stdout=output, stderr=subprocess.STDOUT, start_new_session=True,
        )
        try:
            code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGTERM)
            process.wait(timeout=15)
            raise
        if code != 0:
            raise RuntimeError(f"Command failed with exit {code}; see ignored log {log.name}")


class ScreenRecorder:
    """Continuously record one phase, rolling at Android's 180-second limit."""

    def __init__(self, output_dir: Path, run_id: str, phase: str) -> None:
        self.output_dir = output_dir
        self.run_id = run_id
        self.phase = phase
        self.remote_dir = f"/sdcard/Movies/walk020-{run_id}-{phase}"
        self.stop_requested = threading.Event()
        self.thread = threading.Thread(target=self._record, name=f"screenrecord-{phase}", daemon=True)
        self.error: BaseException | None = None
        self.clips: list[dict[str, Any]] = []
        self.started_at = utc_now()

    def start(self) -> None:
        self.output_dir.mkdir(parents=True, exist_ok=True)
        checked(["adb", "-s", DEVICE, "shell", "mkdir", "-p", self.remote_dir])
        self.thread.start()
        time.sleep(1.0)
        if self.error:
            raise RuntimeError(f"screenrecord failed to start: {self.error}")

    def _remote_recorder_pid(self) -> str | None:
        output = checked(["adb", "-s", DEVICE, "shell", "pidof", "screenrecord"]).strip()
        if not output:
            return None
        pids = output.split()
        if len(pids) != 1:
            raise RuntimeError("Refusing to signal an ambiguous screenrecord PID set")
        return pids[0]

    def _record(self) -> None:
        try:
            index = 0
            while not self.stop_requested.is_set():
                index += 1
                remote = f"{self.remote_dir}/clip-{index:03d}.mp4"
                log = self.output_dir / f"{self.phase}-clip-{index:03d}.screenrecord.log"
                clip: dict[str, Any] = {
                    "index": index,
                    "remoteName": f"clip-{index:03d}.mp4",
                    "startedAt": utc_now(),
                    "monotonicStart": time.monotonic(),
                    "durationLimitSeconds": 180,
                }
                with log.open("wb") as output:
                    process = adb(
                        "shell", "screenrecord", "--bit-rate", "12000000", "--time-limit", "180", remote,
                        stdout=output,
                    )
                    remote_pid: str | None = None
                    discovery_deadline = time.monotonic() + 10
                    while process.poll() is None and not self.stop_requested.is_set():
                        if remote_pid is None and time.monotonic() <= discovery_deadline:
                            remote_pid = self._remote_recorder_pid()
                        time.sleep(0.05)
                    if self.stop_requested.is_set() and process.poll() is None:
                        # SIGINT the exact on-device recorder, not the local adb
                        # client. This lets the MP4 muxer finalize every frame.
                        if remote_pid is None:
                            remote_pid = self._remote_recorder_pid()
                        if remote_pid is None:
                            raise RuntimeError("Could not identify the owned on-device screenrecord PID")
                        checked(["adb", "-s", DEVICE, "shell", "kill", "-INT", remote_pid])
                        try:
                            process.wait(timeout=20)
                        except subprocess.TimeoutExpired:
                            os.killpg(process.pid, signal.SIGTERM)
                            process.wait(timeout=10)
                            raise RuntimeError("On-device screenrecord did not finalize after SIGINT")
                    code = process.wait()
                clip["exitCode"] = code
                clip["endedAt"] = utc_now()
                clip["monotonicEnd"] = time.monotonic()
                clip["recorderWallSeconds"] = round(clip["monotonicEnd"] - clip["monotonicStart"], 6)
                clip.pop("monotonicStart")
                clip.pop("monotonicEnd")
                self.clips.append(clip)
                if code != 0:
                    raise RuntimeError(f"adb screenrecord clip {index} exited {code}")
        except BaseException as error:
            self.error = error

    def stop(self) -> dict[str, Any]:
        self.stop_requested.set()
        self.thread.join(timeout=210)
        if self.thread.is_alive():
            raise RuntimeError("screenrecord thread did not stop")
        if self.error:
            raise RuntimeError(f"screenrecord failed: {self.error}")
        for clip in self.clips:
            local_name = f"{self.phase}-clip-{clip['index']:03d}.mp4"
            local = self.output_dir / local_name
            checked(["adb", "-s", DEVICE, "pull", f"{self.remote_dir}/{clip['remoteName']}", str(local)], timeout=90)
            checked(["adb", "-s", DEVICE, "shell", "rm", "-f", f"{self.remote_dir}/{clip['remoteName']}"])
            clip["localFile"] = str(local.relative_to(REPO))
            clip["bytes"] = local.stat().st_size
            clip["sha256"] = digest(local)
        checked(["adb", "-s", DEVICE, "shell", "rmdir", self.remote_dir])
        summary = {
            "schemaVersion": 1,
            "ticket": "WALK-020",
            "phase": self.phase,
            "avd": AVD,
            "device": DEVICE,
            "captureScope": "Continuous real emulator UI via adb screenrecord; sequential clips at the 180-second platform limit",
            "startedAt": self.started_at,
            "endedAt": utc_now(),
            "clipCount": len(self.clips),
            "clips": self.clips,
        }
        path = self.output_dir / f"{self.phase}-capture.json"
        path.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
        return summary


def ensure_stable_device() -> None:
    state = checked(["adb", "-s", DEVICE, "get-state"]).strip()
    if state != "device":
        raise RuntimeError(f"Authorized {AVD} is not online: {state}")
    avd_output = checked(["adb", "-s", DEVICE, "emu", "avd", "name"]).strip()
    avd = avd_output.splitlines()[0].strip() if avd_output else ""
    if avd != AVD:
        raise RuntimeError(f"Refusing unexpected Android AVD {avd!r}; only {AVD!r} is authorized")
    sdk = checked(["adb", "-s", DEVICE, "shell", "getprop", "ro.build.version.sdk"]).strip()
    if sdk != "34":
        raise RuntimeError(f"Refusing unexpected Android API {sdk!r}; only API 34 is authorized")
    checked(["sh", "scripts/fixture-mode.sh", "stable"])


def load_receipt(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise RuntimeError(f"Missing or invalid CLI JSON receipt: {error}") from error
    if not isinstance(value, dict):
        raise RuntimeError("CLI receipt is not an object")
    return value


def record_phase(run_id: str, phase: str, goal: str,
                 repair_flow: Path | None = None) -> dict[str, Any]:
    output_dir = RAW_ROOT / run_id
    output_dir.mkdir(parents=True, exist_ok=True)
    ensure_stable_device()
    command_name = "explore" if phase == "cold" else "remember" if phase == "repair-baseline" else "run"
    cli = [
        "bun", "../../apps/cli/walkcache.ts", command_name, goal,
        "--config", "walkcache.config.json", "--json",
    ]
    if phase == "repair-baseline":
        if repair_flow is None or not repair_flow.is_file():
            raise RuntimeError("repair-baseline requires an existing model-authored --repair-flow")
        source_candidate = repair_flow.resolve()
        source_candidate.relative_to(REPO)
        source_receipt = load_receipt(source_candidate.parent / "cold.receipt.json")
        if (source_receipt.get("command") != "explore" or not source_receipt.get("passed") or
                not source_receipt.get("modelCalls") or
                source_receipt.get("yamlSha256") != digest(source_candidate)):
            raise RuntimeError("Repair fixture is not a passing, hash-matching model-authored cold YAML")
        shutil.copyfile(source_candidate, output_dir / "repair-fixture-candidate.yaml")
        (output_dir / "repair-fixture-provenance.json").write_text(json.dumps({
            "sourceRunId": source_candidate.parent.name,
            "sourceGoal": source_receipt["identity"]["goal"],
            "sourceModelCalls": source_receipt["modelCalls"],
            "sourceYamlSha256": source_receipt["yamlSha256"],
            "candidateBytesCopiedExactly": True,
            "baselineGoal": goal,
            "baselineCommand": "remember",
            "separateFromFreshColdWarmLifecycle": True,
        }, indent=2) + "\n")
        attempt = SPIKE / ".walkcache" / "attempts" / f"walk020-{run_id}-repair-fixture"
        attempt.mkdir(parents=True, exist_ok=False)
        attempt_candidate = attempt / "candidate.yaml"
        shutil.copyfile(source_candidate, attempt_candidate)
        cli.extend(["--flow", str(attempt_candidate.relative_to(SPIKE))])
    receipt_path = output_dir / f"{phase}.receipt.json"
    stderr_path = output_dir / f"{phase}.stderr.log"
    recorder = ScreenRecorder(output_dir, run_id, phase)
    recorder.start()
    started_at = utc_now()
    try:
        with receipt_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
            process = subprocess.Popen(
                cli, cwd=SPIKE, env=android_environment(), stdout=stdout, stderr=stderr,
                start_new_session=True,
            )
            timeout = 960 if phase == "cold" else 330
            try:
                cli_code = process.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=20)
                raise RuntimeError(f"{phase} CLI exceeded {timeout}s")
    finally:
        capture = recorder.stop()
    receipt = load_receipt(receipt_path)
    if receipt.get("yamlPath"):
        source = SPIKE / receipt["yamlPath"]
        if digest(source) != receipt.get("yamlSha256"):
            raise RuntimeError(f"{phase} saved YAML hash differs from its receipt")
        shutil.copyfile(source, output_dir / f"{phase}-saved-checkout.yaml")
    if phase in {"cold", "repair-baseline"}:
        memory = json.loads((SPIKE / ".walkcache" / "memory" / "walks.json").read_text(encoding="utf-8"))
        entries = [
            walk for walk in memory["walks"]
            if walk.get("goalKey") == receipt["goalKey"] and
            walk.get("buildFingerprint") == receipt["identity"]["buildFingerprint"] and
            walk.get("flowSha256") == receipt["yamlSha256"]
        ]
        expected_origin = "explore" if phase == "cold" else "remember"
        if (len(entries) != 1 or entries[0].get("origin") != expected_origin or
                entries[0]["lastValidation"].get("passed") is not True):
            raise RuntimeError(f"{phase} independently verified entry is missing from local memory")
        (output_dir / f"{phase}-stored-memory.json").write_text(json.dumps(entries[0], indent=2) + "\n")
    metadata = {
        "schemaVersion": 1,
        "ticket": "WALK-020",
        "runId": run_id,
        "phase": phase,
        "goal": goal,
        "avd": AVD,
        "device": DEVICE,
        "startedAt": started_at,
        "endedAt": utc_now(),
        "workingDirectory": "spikes/001-android-walkcache",
        "command": cli,
        "cliExitCode": cli_code,
        "receipt": {
            key: receipt.get(key) for key in [
                "command", "outcome", "passed", "cacheStatus", "modelCalls", "modelCallBasis",
                "durationMs", "assertions", "repairCount", "yamlPath", "yamlSha256", "report",
            ]
        },
        "capture": capture,
    }
    path = output_dir / f"{phase}.session.json"
    path.write_text(json.dumps(metadata, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({key: value for key, value in metadata.items() if key != "capture"}, indent=2))
    if cli_code != 0:
        raise RuntimeError(f"{phase} CLI exited {cli_code}; raw take/report preserved")
    return metadata


def selector_hashes() -> dict[str, str]:
    return {str(path): digest(SPIKE / path) for path in SELECTOR_FILES}


def require_clean_selector_files() -> None:
    result = subprocess.run(
        ["git", "diff", "--exit-code", "--", *[str(Path("spikes/001-android-walkcache") / path) for path in SELECTOR_FILES]],
        cwd=REPO, env=android_environment(),
    )
    if result.returncode != 0:
        raise RuntimeError("Selector files already contain changes; refusing to overwrite another agent's work")


def build_and_install(log_name: str, run_id: str) -> None:
    output = RAW_ROOT / run_id / log_name
    logged(["./gradlew", ":app:assembleDebug"], output)
    with output.open("ab") as log:
        process = subprocess.Popen(
            ["adb", "-s", DEVICE, "install", "-r", "-g", "app/build/outputs/apk/debug/app-debug.apk"],
            cwd=SPIKE, env=android_environment(), stdout=log, stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        code = process.wait(timeout=120)
        if code != 0:
            raise RuntimeError(f"adb install exited {code} for {log_name}")


def stale_repair(run_id: str, goal: str) -> dict[str, Any]:
    output_dir = RAW_ROOT / run_id
    warm = load_receipt(output_dir / "repair-warm.receipt.json")
    if not warm.get("passed") or warm.get("cacheStatus") != "hit" or warm.get("modelCalls") != 0:
        raise RuntimeError("Repair-fixture warm prerequisite is not a passing exact zero-model cache hit")
    baseline = load_receipt(output_dir / "repair-baseline.receipt.json")
    cold_yaml = (output_dir / "repair-baseline-saved-checkout.yaml").read_text(encoding="utf-8")
    if "Order #WC-1001 · Mochi sticker pack" not in cold_yaml:
        raise RuntimeError("Model-authored repair YAML lacks the required unchanged final product assertion")
    if baseline.get("command") != "remember" or not baseline.get("passed"):
        raise RuntimeError("Repair fixture was not independently remembered/verified")
    if (baseline.get("yamlSha256") != warm.get("yamlSha256") or
            baseline.get("identity") != warm.get("identity")):
        raise RuntimeError("Repair baseline and repair-fixture warm identities differ")
    # The configured repairer accepts exactly this one validated failing Maestro
    # step. Do not change model-authored assertions or broaden that classifier:
    # reject an incompatible take before changing/installing the stale source.
    lines = cold_yaml.splitlines()
    references = [index for index, line in enumerate(lines) if "checkout_button" in line]
    if not references:
        raise RuntimeError("Model-authored flow has no checkout_button step; refusing stale probe")
    first = references[0]
    action_index = next((index for index in range(first, -1, -1) if lines[index].startswith("- ")), None)
    multiline_tap = (
        action_index is not None and lines[action_index] == "- tapOn:" and
        lines[first].strip() in {
            'id: "dev.walkcache.checkout:id/checkout_button"',
            "id: dev.walkcache.checkout:id/checkout_button",
        }
    )
    inline_tap = first == action_index and lines[first].strip() in {
        '- tapOn: "dev.walkcache.checkout:id/checkout_button"',
        "- tapOn: dev.walkcache.checkout:id/checkout_button",
    }
    if not (multiline_tap or inline_tap):
        raise RuntimeError(
            "Cold flow's first checkout_button reference is not the validated tapOn; "
            "rejecting this take before mutation without editing any assertion"
        )
    (output_dir / "stale-repair-preflight.json").write_text(json.dumps({
        "firstCheckoutReferenceLine": first + 1,
        "expectedFailingStep": "Tap on id: dev.walkcache.checkout:id/checkout_button",
        "assertionsEdited": False,
        "classifierBroadened": False,
        "mutationAllowed": True,
    }, indent=2) + "\n")
    require_clean_selector_files()
    baseline_dir = output_dir / "baseline-selector-source"
    baseline_dir.mkdir(parents=True, exist_ok=True)
    baselines: dict[str, bytes] = {}
    for relative in SELECTOR_FILES:
        source = SPIKE / relative
        baselines[str(relative)] = source.read_bytes()
        destination = baseline_dir / relative.name
        shutil.copyfile(source, destination)
    baseline_hashes = selector_hashes()
    (output_dir / "baseline-selector-hashes.json").write_text(json.dumps(baseline_hashes, indent=2) + "\n")

    mutated = False
    try:
        # This is the exact established experiment-only two-file reversal. It
        # is never committed, and no other working-tree file is touched.
        # The reviewed patch's trailing blank-line context is rejected by
        # current Git apply even though historical fixture hashes match.
        # BSD patch applies the identical reverse hunks without creating
        # backup files; its dry-run was checked before this driver was used.
        logged(["patch", "--no-backup-if-mismatch", "-R", "-p1", "-i", "evidence/walk007-repair.diff"], output_dir / "stale-source-probe.log")
        mutated = True
        build_and_install("stale-build-install.log", run_id)
        metadata = record_phase(run_id, "stale-repair", goal)
        if selector_hashes() != baseline_hashes:
            raise RuntimeError("Bounded repair did not restore the exact normal selector bytes")
        return metadata
    except BaseException:
        # Byte restoration cannot overwrite unrelated edits: this block runs
        # only after the pre-probe clean-file check and our own exact reversal.
        for relative, value in baselines.items():
            (SPIKE / relative).write_bytes(value)
        if mutated:
            build_and_install("restored-after-failure-build-install.log", run_id)
        raise
    finally:
        checked(["sh", "scripts/fixture-mode.sh", "stable"])


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--phase", choices=[
        "cold", "warm", "repair-baseline", "repair-warm", "stale-repair", "replay",
    ], required=True)
    parser.add_argument("--run-id", help="Reuse the cold run ID for every later phase")
    parser.add_argument("--goal", help="Goal to record; use exact 'checkout' for the configured repair policy")
    parser.add_argument("--repair-flow", type=Path,
                        help="Hash-preserved model-authored cold YAML for the separate repair baseline")
    args = parser.parse_args()
    if args.phase != "cold" and not args.run_id:
        parser.error("--run-id is required after cold")
    run_id = args.run_id or datetime.now(timezone.utc).strftime("walk020-android-%Y%m%dT%H%M%S") + f"-{os.getpid()}"
    if not RUN_ID.fullmatch(run_id):
        parser.error("run-id must be 3–91 URL-safe letters/numbers/hyphens")
    raw = RAW_ROOT / run_id
    if args.phase == "cold" and raw.exists():
        parser.error("refusing to overwrite an existing cold take directory")
    goal_file = raw / "goal.txt"
    if args.phase == "cold":
        goal = args.goal.strip() if args.goal else f"checkout live walk020 {run_id}"
        if not goal or len(goal) > 120:
            parser.error("goal must be 1–120 characters")
        raw.mkdir(parents=True, exist_ok=False)
        goal_file.write_text(goal + "\n", encoding="utf-8")
    else:
        if not goal_file.is_file():
            parser.error("cold goal.txt is missing for this run-id")
        saved_goal = goal_file.read_text(encoding="utf-8").strip()
        if args.goal and args.goal.strip() != saved_goal:
            parser.error(f"goal differs from cold run goal: {saved_goal!r}")
        goal = saved_goal
    if args.phase == "stale-repair":
        if args.repair_flow:
            parser.error("--repair-flow is used only by repair-baseline")
        stale_repair(run_id, goal)
    else:
        if args.repair_flow and args.phase != "repair-baseline":
            parser.error("--repair-flow is used only by repair-baseline")
        record_phase(run_id, args.phase, goal, args.repair_flow)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
