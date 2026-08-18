#!/usr/bin/env python3
"""Mock weather service for the credential-register smoke.

Serves exactly one endpoint:

    GET /weather?city=<name>

Authentication: the `X-Api-Key` header must equal $MOCK_API_KEY (default:
the demo secret). Anything else gets 401 with a non-revealing body.

Determinism: weather values are derived from a stable hash of the city
name, so the verdict can assert exact expected values without a clock or
randomness.

Leak discipline: the access log prints method, path, status and the auth
VERDICT (ok/bad-key/missing-key) — never header values, never the secret.
The log is what proves the gateway injected the right header.
"""

from __future__ import annotations

import hashlib
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

API_KEY = os.environ.get("MOCK_API_KEY", "cred-demo-secret-7f3a")


def weather_for(city: str) -> dict:
    h = hashlib.sha256(city.encode("utf-8")).digest()
    temp = (h[0] / 255.0) * 40 - 5          # -5.0 .. 35.0
    humid = 30 + (h[1] / 255.0) * 60         # 30 .. 90
    conds = ["clear sky", "scattered clouds", "overcast", "light rain"][h[2] % 4]
    return {
        "service": "mockweather",
        "city": city,
        "temperature_c": round(temp, 1),
        "humidity_pct": round(humid, 1),
        "conditions": conds,
    }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt: str, *args) -> None:  # silence default stderr log
        pass

    def _reply(self, status: int, payload: dict) -> None:
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:
        url = urlparse(self.path)
        if url.path != "/weather":
            self._reply(404, {"error": "not found"})
            return
        key = self.headers.get("X-Api-Key")
        if key is None:
            print(f"GET {self.path} -> 401 missing-key", flush=True)
            self._reply(401, {"error": "missing api key"})
            return
        if key != API_KEY:
            # Never echo the received key back — log and body stay secret-free.
            print(f"GET {self.path} -> 401 bad-key", flush=True)
            self._reply(401, {"error": "invalid api key"})
            return
        city = (parse_qs(url.query).get("city") or [""])[0].strip()
        if not city:
            print(f"GET {self.path} -> 400 missing-city", flush=True)
            self._reply(400, {"error": "missing city parameter"})
            return
        print(f"GET {self.path} -> 200 ok", flush=True)
        self._reply(200, weather_for(city))


def main() -> int:
    host = "127.0.0.1"
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 4390
    server = ThreadingHTTPServer((host, port), Handler)
    print(f"mockweather listening on http://{host}:{port} (auth: X-Api-Key header)",
          flush=True)
    server.serve_forever()
    return 0


if __name__ == "__main__":
    sys.exit(main())
