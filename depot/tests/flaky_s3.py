#!/usr/bin/env python3
"""An S3 proxy that loses replies: every Nth conditional PUT (If-Match or
If-None-Match) is forwarded to the real store, which commits it, and then
the client is told nothing (the connection drops) or a 500. This is what a
flaky network or an overloaded store does to a write that did land, and
the server must still neither lose nor double-apply anything.

  python3 tests/flaky_s3.py <listen-port> <upstream-host:port> [every]

Request bytes, the Host header included, pass through untouched, so SigV4
signatures stay valid. Counts go to stdout as `lost N` lines."""
import http.client
import http.server
import sys
import threading

listen, upstream = int(sys.argv[1]), sys.argv[2]
every = int(sys.argv[3]) if len(sys.argv) > 3 else 3
n = 0
lost = 0
lock = threading.Lock()


class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def _go(self):
        global n, lost
        length = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(length) if length else b""
        host, port = upstream.split(":")
        c = http.client.HTTPConnection(host, int(port), timeout=60)
        c.putrequest(self.command, self.path, skip_host=True, skip_accept_encoding=True)
        for k, v in self.headers.items():
            c.putheader(k, v)
        c.endheaders()
        if body:
            c.send(body)
        r = c.getresponse()
        data = r.read()
        cond = self.command == "PUT" and ("if-match" in self.headers or "if-none-match" in self.headers)
        if cond and r.status in (200, 201):
            with lock:
                n += 1
                k = n
            if k % every == 0:
                with lock:
                    lost += 1
                    print(f"lost {lost}", flush=True)
                if (k // every) % 2:
                    self.close_connection = True
                    self.wfile.flush()
                    self.connection.close()  # the write landed; the reply never arrives
                    return
                self.send_response(500)
                self.send_header("content-length", "0")
                self.end_headers()
                return
        self.send_response(r.status)
        for k2, v in r.getheaders():
            if k2.lower() not in ("transfer-encoding", "connection", "content-length"):
                self.send_header(k2, v)
        # a HEAD answer's length is the object's, not its (empty) body's
        self.send_header("content-length", r.getheader("content-length", "0") if self.command == "HEAD" else str(len(data)))
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(data)

    do_GET = do_PUT = do_POST = do_DELETE = do_HEAD = _go

    def log_message(self, *a):
        pass


http.server.ThreadingHTTPServer(("127.0.0.1", listen), H).serve_forever()
