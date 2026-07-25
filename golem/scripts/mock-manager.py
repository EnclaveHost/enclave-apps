#!/usr/bin/env python3
"""Minimal stand-in for the in-enclave encrypted-volumes manager, for LOCAL
golem testing only: reports one always-unlocked volume named "demo" and
answers ok to unlock/sync/lock. Pair it with `wasmtime serve` (see README).
The real manager decrypts ciphertext from your bucket; this one just lets the
UI render the machine room over a plain preopened directory."""
import json
from http.server import BaseHTTPRequestHandler, HTTPServer

STATE = {"volumes": [{"name": "demo", "status": "unlocked",
                      "endpoint": "http://mock.local", "bucket": "mock-bucket",
                      "path": "vols/demo"}]}

class H(BaseHTTPRequestHandler):
    def _send(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self._send(200, STATE)

    def do_POST(self):
        n = int(self.headers.get("content-length") or 0)
        if n:
            self.rfile.read(n)
        self._send(200, {"ok": True})

    def log_message(self, *a):
        pass

if __name__ == "__main__":
    print("mock encrypted-volumes manager on http://127.0.0.1:8391")
    HTTPServer(("127.0.0.1", 8391), H).serve_forever()
