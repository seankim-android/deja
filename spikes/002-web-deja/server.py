#!/usr/bin/env python3
"""Deterministic local Web fixture server for spikes/002-web-deja (pure Python stdlib)."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import mimetypes
import os
from pathlib import Path
import re
from urllib.parse import urlsplit

ROOT = (Path(__file__).resolve().parent / "public").resolve()
PORT = int(os.environ.get("DEJA_WEB_PORT", "4174"))
HOSTNAME = os.environ.get("DEJA_WEB_HOST", "127.0.0.1")
ORDER_NUMBER = os.environ.get("DEJA_FIXTURE_ORDER", "WC-1001")

if not re.fullmatch(r"WC-[0-9]{4}", ORDER_NUMBER):
    raise ValueError("Fixture order must match WC-0000")

CSP = "default-src 'self'; script-src 'self'; style-src 'self'"


class FixtureHandler(BaseHTTPRequestHandler):
    def do_GET(self) -> None:
        self._handle(send_body=True)

    def do_HEAD(self) -> None:
        self._handle(send_body=False)

    def _handle(self, *, send_body: bool) -> None:
        pathname = urlsplit(self.path).path
        if pathname == "/api/order":
            body = json.dumps({"orderNumber": ORDER_NUMBER}).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Cache-Control", "no-store")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            if send_body:
                self.wfile.write(body)
            return

        requested = "/index.html" if pathname == "/" else pathname
        candidate = (ROOT / requested.lstrip("/")).resolve()
        try:
            candidate.relative_to(ROOT)
        except ValueError:
            self._send_not_found(send_body=send_body)
            return

        if not candidate.is_file():
            self._send_not_found(send_body=send_body)
            return

        data = candidate.read_bytes()
        content_type, _ = mimetypes.guess_type(candidate.name)
        self.send_response(200)
        self.send_header("Content-Type", content_type or "application/octet-stream")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Security-Policy", CSP)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        if send_body:
            self.wfile.write(data)

    def _send_not_found(self, *, send_body: bool) -> None:
        body = b"Not found"
        self.send_response(404)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if send_body:
            self.wfile.write(body)

    def log_message(self, format: str, *args: object) -> None:
        return


def main() -> None:
    server = ThreadingHTTPServer((HOSTNAME, PORT), FixtureHandler)
    print(f"Déjà deterministic web fixture: http://{HOSTNAME}:{PORT}/", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
