"""A stub jot deployment: the notebook API in a dict, faithful to the routes
and error shapes the real app serves (see enclave-apps/jot). Enough to prove
the client-side tools speak the contract; the app's own scripts/e2e.sh proves
the server against a real bucket."""

from __future__ import annotations

import json
import threading
import urllib.parse
from http.server import BaseHTTPRequestHandler, HTTPServer

KEY = "stub-key"


def start(key: str = KEY):
    notes: dict[str, str] = {}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *a):  # quiet
            pass

        def _json(self, code: int, body: dict) -> None:
            data = json.dumps(body).encode()
            self.send_response(code)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def _err(self, code: int, msg: str) -> None:
            self._json(code, {"error": {"message": msg}})

        def _body(self) -> dict:
            n = int(self.headers.get("content-length", "0"))
            return json.loads(self.rfile.read(n).decode()) if n else {}

        def _route(self):
            u = urllib.parse.urlsplit(self.path)
            q = dict(urllib.parse.parse_qsl(u.query))
            if u.path.startswith("/api/") and u.path not in ("/api/status", "/api/tools"):
                if self.headers.get("authorization") != "Bearer " + key:
                    return self._err(401, "unauthorized")
            if u.path == "/api/notes" and self.command == "GET":
                p = q.get("prefix", "")
                return self._json(200, {"notes": [
                    {"name": n, "size": len(c.encode()), "modified": "2026-09-01T00:00:00Z"}
                    for n, c in sorted(notes.items()) if n.startswith(p)], "truncated": False})
            if u.path == "/api/search" and self.command == "GET":
                needle = q.get("q", "").lower()
                if not needle:
                    return self._err(400, "q is required")
                hits = []
                for n, c in sorted(notes.items()):
                    if not n.startswith(q.get("prefix", "")):
                        continue
                    for i, line in enumerate(c.splitlines(), 1):
                        if needle in line.lower():
                            hits.append({"name": n, "line": i, "text": line.strip()[:200]})
                return self._json(200, {"query": needle, "hits": hits, "scanned": len(notes),
                                        "skipped": 0, "truncated": False})
            if not u.path.startswith("/api/notes/"):
                return self._err(404, "not found")
            rest = u.path[len("/api/notes/"):]
            verb = ""
            for v in ("append", "delete"):
                if rest.endswith("/" + v):
                    rest, verb = rest[:-len(v) - 1], v
            name = urllib.parse.unquote(rest)
            if ".." in name.split("/") or not name:
                return self._err(400, "bad note name")
            if self.command == "GET" and not verb:
                if name not in notes:
                    return self._err(404, "no such note")
                return self._json(200, {"name": name, "content": notes[name],
                                        "size": len(notes[name].encode()), "etag": "e", "modified": ""})
            if self.command in ("PUT", "POST") and not verb:
                notes[name] = self._body()["content"]
                return self._json(200, {"ok": True, "name": name, "size": len(notes[name].encode()), "etag": "e"})
            if self.command == "POST" and verb == "append":
                cur = notes.get(name, "")
                if cur and not cur.endswith("\n"):
                    cur += "\n"
                cur += self._body()["content"]
                if not cur.endswith("\n"):
                    cur += "\n"
                notes[name] = cur
                return self._json(200, {"ok": True, "name": name, "size": len(cur.encode()), "etag": "e"})
            if (self.command == "DELETE" and not verb) or (self.command == "POST" and verb == "delete"):
                notes.pop(name, None)
                return self._json(200, {"ok": True, "name": name})
            return self._err(405, "method not allowed")

        do_GET = do_PUT = do_POST = do_DELETE = _route

    server = HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, f"http://127.0.0.1:{server.server_port}", notes
