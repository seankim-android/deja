#!/usr/bin/env python3
"""Run real headed Maestro Chromium and continuously record that exact browser.

The Web fixture and candidate YAML are the committed WALK-010 files. Each new
run-id creates a new goal/cache identity; nothing is deleted from prior cache.
The same run-id is then used for warm `run`. Raw outputs stay under ignored
`demo/generated/browser/` and unrelated Android/iOS resources are never used.
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
import subprocess
import sys
import time
from typing import Any

REPO = Path(__file__).resolve().parents[2]
SPIKE = REPO / "spikes" / "002-web-walkcache"
RAW_ROOT = REPO / "demo" / "generated" / "browser"
CAPTURE_SCRIPT = REPO / "demo" / "live" / "cdp_screencast.py"
CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
RUN_ID = re.compile(r"^[a-zA-Z0-9][a-zA-Z0-9-]{2,90}$")


def run_command(command: list[str]) -> str:
    return subprocess.run(command, check=True, text=True, stdout=subprocess.PIPE).stdout


def chrome_pids() -> set[int]:
    output = run_command(["ps", "-axo", "pid=,command="])
    return {
        int(line.split(None, 1)[0])
        for line in output.splitlines()
        if CHROME in line and "--test-type=webdriver" in line and "--remote-debugging-port=0" in line
    }


def debug_port(pid: int) -> int | None:
    try:
        output = run_command(["lsof", "-nP", "-a", "-p", str(pid), "-iTCP", "-sTCP:LISTEN"])
    except subprocess.CalledProcessError:
        return None
    match = re.search(r"127\.0\.0\.1:(\d+) \(LISTEN\)", output)
    return int(match.group(1)) if match else None


def wait_for_browser(existing: set[int], cli_process: subprocess.Popen[bytes]) -> tuple[int, int]:
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        new_pids = chrome_pids() - existing
        for pid in sorted(new_pids):
            port = debug_port(pid)
            if port is not None:
                return pid, port
        if cli_process.poll() is not None:
            raise RuntimeError(f"Maestro exited {cli_process.returncode} before Chromium capture attached")
        time.sleep(0.025)
    raise RuntimeError("No headed Maestro Chromium DevTools endpoint appeared within 30 seconds")


def write_headed_config() -> Path:
    path = SPIKE / ".tmp" / "walk020-headed.config.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    source = json.loads((SPIKE / "walkcache.config.json").read_text(encoding="utf-8"))
    # Config paths resolve relative to the config file. Keep the validated
    # fixture root/cache unchanged and only switch the recording run to headed.
    source["workspaceRoot"] = ".."
    source["headless"] = False
    path.write_text(json.dumps(source, indent=2) + "\n", encoding="utf-8")
    return path


def recording_environment() -> dict[str, str]:
    env = os.environ.copy()
    java_home = "/opt/homebrew/opt/openjdk@21"
    env["JAVA_HOME"] = java_home
    env["PATH"] = f"{Path.home()}/.maestro/bin:{java_home}/bin:/opt/homebrew/bin:{env.get('PATH', '')}"
    env["MAESTRO_CLI_NO_ANALYTICS"] = "1"
    env["MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED"] = "true"
    # In particular, do not select or mutate another session's Android device.
    env.pop("ANDROID_HOME", None)
    env.pop("ANDROID_SDK_ROOT", None)
    env.pop("WALKCACHE_DEVICE", None)
    return env


def load_receipt(path: Path) -> dict[str, Any]:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise RuntimeError(f"Maestro CLI did not emit a readable JSON receipt: {error}") from error


def copy_saved_yaml(receipt: dict[str, Any], destination: Path) -> None:
    yaml_path = receipt.get("yamlPath")
    if not isinstance(yaml_path, str):
        raise RuntimeError("Successful remember/run receipt has no saved YAML path")
    source = SPIKE / yaml_path
    digest = hashlib.sha256(source.read_bytes()).hexdigest()
    if digest != receipt.get("yamlSha256"):
        raise RuntimeError("Saved YAML hash does not match the CLI receipt")
    shutil.copyfile(source, destination)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--phase", choices=["remember", "run"], required=True)
    parser.add_argument("--run-id", help="Reuse this ID for remember and warm run; omit on remember")
    args = parser.parse_args()
    if args.phase == "run" and not args.run_id:
        parser.error("warm run requires --run-id from its remember capture")
    run_id = args.run_id or datetime.now(timezone.utc).strftime("walk020-%Y%m%dT%H%M%S") + f"-{os.getpid()}"
    if not RUN_ID.fullmatch(run_id):
        parser.error("run-id must be 3–91 URL-safe letters/numbers/hyphens")
    output_dir = RAW_ROOT / run_id
    output_dir.mkdir(parents=True, exist_ok=True)
    goal = f"checkout live walk020 {run_id}"
    label = "BEFORE CACHE" if args.phase == "remember" else "AFTER CACHE"
    raw_video = output_dir / f"{args.phase}.mp4"
    if raw_video.exists():
        parser.error(f"refusing to overwrite an existing raw capture: {raw_video.relative_to(REPO)}")

    config = write_headed_config()
    stdout_path = output_dir / f"{args.phase}.receipt.json"
    stderr_path = output_dir / f"{args.phase}.stderr.log"
    recorder_stdout = output_dir / f"{args.phase}.recorder-summary.json"
    recorder_stderr = output_dir / f"{args.phase}.recorder.stderr.log"
    metadata_path = output_dir / f"{args.phase}.session.json"

    cli = [
        "bun", str(REPO / "apps" / "cli" / "walkcache.ts"), args.phase, goal,
        *(["--flow", "evidence/checkout.yaml"] if args.phase == "remember" else []),
        "--config", str(config), "--json",
    ]
    existing = chrome_pids()
    started_at = datetime.now(timezone.utc).isoformat()
    recorder: subprocess.Popen[bytes] | None = None
    browser_pid: int | None = None
    browser_port: int | None = None
    with stdout_path.open("wb") as cli_out, stderr_path.open("wb") as cli_err:
        process = subprocess.Popen(
            cli, cwd=SPIKE, env=recording_environment(),
            stdout=cli_out, stderr=cli_err, start_new_session=True,
        )
        try:
            browser_pid, browser_port = wait_for_browser(existing, process)
            with recorder_stdout.open("wb") as rec_out, recorder_stderr.open("wb") as rec_err:
                recorder = subprocess.Popen([
                    sys.executable, str(CAPTURE_SCRIPT), "--port", str(browser_port),
                    "--pid", str(browser_pid), "--output", str(raw_video),
                    "--label", label, "--duration", "150", "--fps", "30",
                ], cwd=REPO, stdout=rec_out, stderr=rec_err, start_new_session=True)
            cli_code = process.wait(timeout=150)
        except BaseException:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
            if recorder is not None and recorder.poll() is None:
                recorder.terminate()
                recorder.wait(timeout=15)
            raise

    assert recorder is not None
    try:
        recorder_code = recorder.wait(timeout=45)
    except subprocess.TimeoutExpired:
        recorder.terminate()
        recorder_code = recorder.wait(timeout=15)
        raise RuntimeError("Chromium recorder did not stop after the Maestro browser exited")
    receipt = load_receipt(stdout_path)
    if cli_code == 0:
        copy_saved_yaml(receipt, output_dir / f"{args.phase}-saved-checkout.yaml")
    metadata = {
        "schemaVersion": 1,
        "ticket": "WALK-020",
        "runId": run_id,
        "phase": args.phase,
        "label": label,
        "goal": goal,
        "startedAt": started_at,
        "endedAt": datetime.now(timezone.utc).isoformat(),
        "relativeFixtureDirectory": "spikes/002-web-walkcache",
        "relativeConfig": ".tmp/walk020-headed.config.json (headed recording copy)",
        "command": [
            "bun", "apps/cli/walkcache.ts", args.phase, goal,
            *(["--flow", "evidence/checkout.yaml"] if args.phase == "remember" else []),
            "--config", ".tmp/walk020-headed.config.json", "--json",
        ],
        "cliExitCode": cli_code,
        "recorderExitCode": recorder_code,
        "rawVideo": str(raw_video.relative_to(REPO)),
        "receipt": {
            key: receipt.get(key) for key in [
                "command", "outcome", "passed", "cacheStatus", "modelCalls", "modelCallBasis",
                "durationMs", "assertions", "repairCount", "yamlPath", "yamlSha256", "report",
            ]
        },
    }
    metadata_path.write_text(json.dumps(metadata, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(metadata, indent=2))
    if recorder_code != 0:
        return 5
    return cli_code


if __name__ == "__main__":
    raise SystemExit(main())
