#!/usr/bin/env python3
"""Record a real Chromium page's CDP screencast as an uncut MP4.

This is browser-backed capture, not macOS desktop capture. Chromium emits JPEG
screencast frames while the real page changes; between events this recorder
holds the latest live frame so static waits remain visible at the requested
output frame rate. Frame receipts and hold times are retained in a JSON sidecar.
No screenshots are staged or interpolated into a synthetic journey.
"""

from __future__ import annotations

import argparse
import asyncio
import base64
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import signal
import subprocess
import threading
import time
from typing import Any
from urllib.request import urlopen

import websockets


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def pid_running(pid: int | None) -> bool:
    if pid is None:
        return True
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def page_target(port: int) -> dict[str, Any]:
    with urlopen(f"http://127.0.0.1:{port}/json/list", timeout=5) as response:
        targets = json.load(response)
    pages = [target for target in targets if target.get("type") == "page"]
    if not pages:
        raise RuntimeError("Chromium exposed no page target for screencast capture")
    # Maestro uses one isolated page. Prefer a real fixture page if Chrome has
    # already navigated, while still allowing capture to begin at data:,.
    return next((target for target in pages if target.get("url", "").startswith("http://127.0.0.1:")), pages[0])


class VideoWriter:
    def __init__(self, output: Path, fps: int) -> None:
        self.output = output
        self.fps = fps
        self.latest_jpeg: bytes | None = None
        self.latest_sequence = 0
        self.emitted_frames = 0
        self.held_frames = 0
        self.lock = threading.Lock()
        self.stop = threading.Event()
        self.process: subprocess.Popen[bytes] | None = None
        self.thread = threading.Thread(target=self._run, name="cdp-video-writer", daemon=True)

    def update(self, jpeg: bytes) -> None:
        with self.lock:
            self.latest_jpeg = jpeg
            self.latest_sequence += 1

    def start(self) -> None:
        command = [
            "ffmpeg", "-hide_banner", "-loglevel", "warning", "-y",
            "-f", "image2pipe", "-framerate", str(self.fps), "-i", "pipe:0",
            "-an", "-c:v", "libx264", "-preset", "veryfast", "-crf", "18",
            "-pix_fmt", "yuv420p", "-movflags", "+faststart", str(self.output),
        ]
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE)
        self.thread.start()

    def _run(self) -> None:
        interval = 1.0 / self.fps
        next_frame = time.monotonic()
        last_written_sequence = -1
        while not self.stop.is_set():
            with self.lock:
                jpeg = self.latest_jpeg
                sequence = self.latest_sequence
            if jpeg is not None:
                assert self.process is not None and self.process.stdin is not None
                try:
                    self.process.stdin.write(jpeg)
                    self.process.stdin.flush()
                except BrokenPipeError:
                    break
                self.emitted_frames += 1
                if sequence == last_written_sequence:
                    self.held_frames += 1
                last_written_sequence = sequence
            next_frame += interval
            time.sleep(max(0.0, next_frame - time.monotonic()))

    def finish(self) -> None:
        self.stop.set()
        self.thread.join(timeout=10)
        assert self.process is not None
        if self.process.stdin is not None:
            self.process.stdin.close()
        code = self.process.wait(timeout=60)
        if code != 0:
            raise RuntimeError(f"FFmpeg exited {code} while encoding Chromium screencast")


async def record(args: argparse.Namespace) -> dict[str, Any]:
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    target = page_target(args.port)
    started = utc_now()
    monotonic_start = time.monotonic()
    writer = VideoWriter(output, args.fps)
    receipts: list[dict[str, Any]] = []
    sequence = 0
    disconnected = False

    async with websockets.connect(
        target["webSocketDebuggerUrl"], max_size=64 * 1024 * 1024,
        ping_interval=10, ping_timeout=20,
    ) as websocket:
        await websocket.send(json.dumps({"id": 1, "method": "Page.enable"}))
        await websocket.send(json.dumps({
            "id": 2,
            "method": "Page.startScreencast",
            "params": {
                "format": "jpeg", "quality": 90,
                "maxWidth": args.max_width, "maxHeight": args.max_height,
                "everyNthFrame": 1,
            },
        }))
        writer.start()

        async def consume() -> None:
            nonlocal sequence, disconnected
            try:
                async for raw in websocket:
                    message = json.loads(raw)
                    if message.get("method") != "Page.screencastFrame":
                        continue
                    params = message["params"]
                    sequence += 1
                    elapsed = time.monotonic() - monotonic_start
                    jpeg = base64.b64decode(params["data"])
                    writer.update(jpeg)
                    receipts.append({
                        "sequence": sequence,
                        "elapsedSeconds": round(elapsed, 6),
                        "bytes": len(jpeg),
                        "metadata": params.get("metadata", {}),
                    })
                    await websocket.send(json.dumps({
                        "id": 1000 + sequence,
                        "method": "Page.screencastFrameAck",
                        "params": {"sessionId": params["sessionId"]},
                    }))
            except websockets.exceptions.ConnectionClosed:
                disconnected = True

        consumer = asyncio.create_task(consume())
        deadline = monotonic_start + args.duration
        try:
            while time.monotonic() < deadline and pid_running(args.pid):
                if disconnected:
                    break
                await asyncio.sleep(0.1)
        finally:
            writer.finish()
            consumer.cancel()
            try:
                await consumer
            except (asyncio.CancelledError, websockets.exceptions.ConnectionClosed):
                pass

    ended = utc_now()
    duration = time.monotonic() - monotonic_start
    summary = {
        "schemaVersion": 1,
        "captureScope": "real Chromium CDP screencast; not macOS desktop/window recording",
        "label": args.label,
        "startedAt": started,
        "endedAt": ended,
        "durationSeconds": round(duration, 6),
        "fps": args.fps,
        "maxWidth": args.max_width,
        "maxHeight": args.max_height,
        "targetUrlAtAttachment": target.get("url"),
        "screencastEventCount": sequence,
        "emittedFrameCount": writer.emitted_frames,
        "heldFrameCount": writer.held_frames,
        "browserDisconnected": disconnected,
        "frames": receipts,
    }
    # Keep the detailed sidecar distinct from the orchestrator's CLI receipt
    # (for example, remember.receipt.json) and recorder stdout summary.
    sidecar = output.with_name(f"{output.stem}.screencast.json")
    sidecar.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
    return {key: value for key, value in summary.items() if key != "frames"}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, required=True, help="Loopback Chromium DevTools port")
    parser.add_argument("--pid", type=int, help="Stop recording when this Chromium process exits")
    parser.add_argument("--output", required=True, help="Ignored raw MP4 output path")
    parser.add_argument("--label", required=True, help="Evidence label, for example BEFORE CACHE")
    parser.add_argument("--duration", type=float, default=150.0, help="Maximum capture seconds")
    parser.add_argument("--fps", type=int, default=30)
    parser.add_argument("--max-width", type=int, default=1280)
    parser.add_argument("--max-height", type=int, default=800)
    args = parser.parse_args()
    if args.duration <= 0 or args.fps <= 0:
        parser.error("duration and fps must be positive")
    summary = asyncio.run(record(args))
    print(json.dumps(summary, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
