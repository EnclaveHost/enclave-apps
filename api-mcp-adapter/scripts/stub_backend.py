#!/usr/bin/env python3
"""A stand-in for the APIs an adapter config points at, for scripts/e2e.sh.

Every route answers with something the checks can pin: echo routes reflect
the request they received (method, path, query, the headers that matter,
the body), the image routes return a real 1x1 PNG / WebP as an OpenAI-style
envelope, the search route returns citable hits, the notes route is
per-user (401 without X-User), /big is bigger than the entry's cap, and
/fail fails. Loopback only.
"""
import base64
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import parse_qs, urlsplit

PNG_1X1 = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg=="
)
WEBP_1X1 = base64.b64decode(
    "UklGRhoAAABXRUJQVlA4TA0AAAAvAAAAEAcQERGIiP4HAA=="
)


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _send(self, status, body, ctype="application/json"):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _body(self):
        n = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(n) if n else b""
        try:
            return json.loads(raw) if raw else None
        except Exception:
            return raw.decode("utf-8", "replace")

    def _echo(self, body):
        u = urlsplit(self.path)
        return {
            "method": self.command,
            "path": u.path,
            "query": {k: v[0] if len(v) == 1 else v for k, v in parse_qs(u.query, keep_blank_values=True).items()},
            "headers": {k: self.headers.get(k) for k in ("x-api-key", "x-user", "authorization", "content-type", "accept") if self.headers.get(k)},
            "body": body,
        }

    def do_GET(self):
        u = urlsplit(self.path)
        if u.path.startswith("/echo"):
            return self._send(200, self._echo(None))
        if u.path == "/search":
            q = parse_qs(u.query).get("q", [""])[0]
            return self._send(200, {"results": [
                {"title": "First hit for " + q, "url": "https://example.com/a"},
                {"title": "", "url": "https://example.com/b"},
                {"title": "no url"},
            ], "text": "two hits about " + q})
        if u.path.startswith("/notes/"):
            if self.headers.get("x-api-key") != "notekey":
                return self._send(401, {"error": {"message": "bad key"}})
            user = self.headers.get("x-user")
            if not user:
                return self._send(401, {"error": {"message": "[sso_required] name the user"}})
            name = u.path[len("/notes/"):]
            return self._send(200, {"name": name, "content": f"note {name} of {user}", "etag": "1"})
        if u.path == "/big":
            return self._send(200, b"x" * (256 * 1024), "text/plain")
        if u.path == "/fail":
            return self._send(500, b"boom", "text/plain")
        if u.path == "/ping":
            return self._send(200, {"ok": True})
        return self._send(404, {"error": {"message": "no such route"}})

    def do_POST(self):
        u = urlsplit(self.path)
        body = self._body()
        if u.path.startswith("/echo"):
            return self._send(200, self._echo(body))
        if u.path == "/v1/images/generations":
            if self.headers.get("authorization") != "Bearer imgkey":
                return self._send(401, {"error": {"message": "bad image key"}})
            if not isinstance(body, dict) or not body.get("prompt"):
                return self._send(400, {"error": {"message": "prompt required"}})
            if body["prompt"] == "slow":
                time.sleep(2)
            if body["prompt"] == "busy":
                return self._send(200, {"error": "busy"})
            return self._send(200, {"data": [{"b64_json": base64.b64encode(PNG_1X1).decode()}], "size": body.get("size")})
        if u.path == "/v1/images/upscale":
            if not isinstance(body, dict) or not str(body.get("image", "")).startswith("data:image/"):
                return self._send(400, {"error": {"message": "image data URI required"}})
            return self._send(200, {"data": [{"b64_json": "data:image/webp;base64," + base64.b64encode(WEBP_1X1).decode()}], "factor": body.get("factor")})
        return self._send(404, {"error": {"message": "no such route"}})

    do_PUT = do_POST
    do_PATCH = do_POST

    def do_DELETE(self):
        u = urlsplit(self.path)
        if u.path.startswith("/echo"):
            return self._send(200, self._echo(None))
        return self._send(404, {"error": {"message": "no such route"}})


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 18520
    HTTPServer(("127.0.0.1", port), H).serve_forever()
