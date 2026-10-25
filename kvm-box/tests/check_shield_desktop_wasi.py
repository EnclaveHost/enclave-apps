#!/usr/bin/env python3
"""Exercise the production Wasip2 component in a private network namespace.
All credentials/payloads are public fixtures. No real desktop/worker is used.
Run with KVMBOX_TEST_PARENT_NETNS set to the parent's network namespace link,
then unshare --user --map-root-user --net python3 wasi-check.py COMPONENT.
"""
import base64
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import socket
import socketserver
import subprocess
import sys
import threading
import time
import unittest

COMPONENT = str(Path(sys.argv.pop(1)).resolve())
HERE = Path(sys.argv.pop(1)).resolve() if len(sys.argv) > 1 else Path(__file__).resolve().parent
HERE.mkdir(parents=True, exist_ok=True)
PASSWORD = "synthetic-wasi-owner-0001"
AUTH = "Basic " + base64.b64encode(("owner:" + PASSWORD).encode()).decode()
RECORDS = []
LOCK = threading.Lock()

def receive_head(s):
    data = b""
    while b"\r\n\r\n" not in data:
        chunk = s.recv(4096)
        if not chunk:
            raise EOFError(data[:200])
        data += chunk
        if len(data) > 1024 * 1024:
            raise ValueError("fixture head too large")
    head, rest = data.split(b"\r\n\r\n", 1)
    return head, rest

def receive_exact(s, n, data=b""):
    while len(data) < n:
        chunk = s.recv(n - len(data))
        if not chunk:
            raise EOFError("truncated fixture stream")
        data += chunk
    return data

class Companion(socketserver.BaseRequestHandler):
    def handle(self):
        s = self.request
        s.settimeout(8)
        try:
            head, rest = receive_head(s)
            first, *lines = head.decode("ascii").split("\r\n")
            headers = {k.lower(): v.strip() for k, v in (line.split(":", 1) for line in lines)}
            target = first.split()[1]
            length = int(headers.get("content-length", "0"))
            body = receive_exact(s, length, rest) if length else b""
            with LOCK:
                RECORDS.append(dict(first=first, headers=headers, bodySha256=hashlib.sha256(body).hexdigest(), bodyBytes=len(body)))
            if target.startswith("/websockify"):
                accept = base64.b64encode(hashlib.sha1((headers["sec-websocket-key"] + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest())
                s.sendall(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: " + accept + b"\r\n\r\n")
            elif target.startswith("/api/moonlight/tunnel"):
                s.sendall(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: kvm-box-tunnel\r\n\r\n")
                if rest:
                    s.sendall(rest)
            else:
                if target == "/slow":
                    time.sleep(1.5)
                content = json.dumps({"status": "stopped", "target": target, "bytes": len(body), "sha256": hashlib.sha256(body).hexdigest()}).encode()
                s.sendall(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: " + str(len(content)).encode() + b"\r\nConnection: close\r\n\r\n" + content)
                return
            while True:
                data = s.recv(4096)
                if not data:
                    s.sendall(b"half-close-observed")
                    return
                s.sendall(data)
        except (OSError, EOFError):
            return

class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

def command():
    return ["/usr/bin/wasmtime", "run", "-C", "cache=n", "-S", "inherit-network=y", "--env", "KVMBOX_OWNER_PASSWORD", "--env", "KVMBOX_ACCESS_SHA256", COMPONENT]

def request(path, method="GET", body=b"", extra="", authorized=True):
    headers = f"Host: fixture.test\r\nConnection: close\r\n"
    if authorized:
        headers += f"Authorization: {AUTH}\r\n"
    if method == "POST":
        headers += f"Content-Length: {len(body)}\r\n"
    wire = f"{method} {path} HTTP/1.1\r\n{headers}{extra}\r\n".encode() + body
    with socket.create_connection(("127.0.0.1", 8000), 4) as s:
        s.settimeout(8)
        s.sendall(wire)
        result = b""
        while True:
            b = s.recv(65536)
            if not b:
                return result
            result += b

class Wasip2Checks(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        parent_netns = os.environ.get("KVMBOX_TEST_PARENT_NETNS")
        if not parent_netns or os.readlink("/proc/self/ns/net") == parent_netns:
            raise RuntimeError("Run in a fresh network namespace; fixed ports must not touch live services")
        subprocess.run(["/usr/bin/ip", "link", "set", "lo", "up"], check=True)
        cls.startup_refusals = []
        for name, values in [
            ("missing", {}), ("empty", {"KVMBOX_OWNER_PASSWORD": ""}),
            ("oversized", {"KVMBOX_OWNER_PASSWORD": "x" * 1025}),
            ("legacy", {"KVMBOX_OWNER_PASSWORD": PASSWORD, "KVMBOX_ACCESS_SHA256": ""}),
        ]:
            r = subprocess.run(command(), env={"PATH": "/usr/bin:/bin", **values}, capture_output=True, timeout=20)
            if r.returncode != 1 or b"refused to start" not in r.stderr:
                raise AssertionError((name, r.returncode, r.stderr))
            if PASSWORD.encode() in r.stderr:
                raise AssertionError("fixture credential leaked")
            cls.startup_refusals.append(name)
        cls.server = Server(("127.0.0.1", 19080), Companion)
        cls.addClassCleanup(cls.server.server_close)
        cls.server_thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.server_thread.start()
        cls.addClassCleanup(cls.server.shutdown)
        cls.log = (HERE / "wasi-runtime.log").open("wb")
        cls.addClassCleanup(cls.log.close)
        cls.proc = subprocess.Popen(command(), env={"PATH": "/usr/bin:/bin", "KVMBOX_OWNER_PASSWORD": PASSWORD}, stdout=cls.log, stderr=subprocess.STDOUT)
        cls.addClassCleanup(cls.stop_component)
        until = time.monotonic() + 15
        while time.monotonic() < until:
            if cls.proc.poll() is not None:
                raise AssertionError(("component exited", cls.proc.returncode))
            try:
                with socket.create_connection(("127.0.0.1", 8000), .2):
                    return
            except OSError:
                time.sleep(.05)
        raise TimeoutError("Wasm listener not ready")

    @classmethod
    def stop_component(cls):
        cls.proc.terminate()
        try:
            cls.proc.wait(timeout=4)
        except subprocess.TimeoutExpired:
            cls.proc.kill(); cls.proc.wait(timeout=4)

    def test_01_auth_before_backend(self):
        with LOCK:
            before = len(RECORDS)
        for path in ["/", "/api/status", "/desktop/vnc.html", "/desktop/websockify"]:
            self.assertTrue(request(path, authorized=False).startswith(b"HTTP/1.1 401"))
        self.assertTrue(request("/api/status", extra=f"Authorization: {AUTH}\r\n").startswith(b"HTTP/1.1 401"))
        with LOCK:
            self.assertEqual(len(RECORDS), before)

    def test_02_dashboard_and_fixed_route(self):
        self.assertIn(b"HTTP/1.1 200", request("/"))
        r = request("/api/status")
        self.assertTrue(r.startswith(b"HTTP/1.1 200"), r)
        self.assertEqual(json.loads(r.split(b"\r\n\r\n", 1)[1])["target"], "/v1/status")

    def test_03_body_streaming_and_credentials(self):
        body = bytes(range(256)) * 8192
        r = request("/desktop/echo", "POST", body, "X-Compute-Authorization: fixture-compute-token\r\nX-Forwarded-For: spoof\r\nX-Internal-Auth: spoof\r\n")
        self.assertTrue(r.startswith(b"HTTP/1.1 200"), r[:200])
        result = json.loads(r.split(b"\r\n\r\n", 1)[1])
        self.assertEqual(result["sha256"], hashlib.sha256(body).hexdigest())
        self.assertEqual(result["bytes"], len(body))
        with LOCK:
            record = RECORDS[-1]
        self.assertEqual(record["headers"]["host"], "127.0.0.1:19080")
        self.assertEqual(record["headers"]["x-compute-authorization"], "fixture-compute-token")
        for key in ["authorization", "x-forwarded-for", "x-internal-auth"]:
            self.assertNotIn(key, record["headers"])

    def tunnel(self, websocket=False, number=0):
        s = socket.create_connection(("127.0.0.1", 8000), 4)
        s.settimeout(6)
        if websocket:
            path = "/desktop/websockify"
            upgrade = "websocket"
            extra = "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n"
        else:
            path = "/desktop/api/moonlight/tunnel?transport=udp&port=47998"
            upgrade = "kvm-box-tunnel"
            extra = ""
        s.sendall(f"GET {path} HTTP/1.1\r\nHost: fixture.test\r\nAuthorization: {AUTH}\r\nConnection: Upgrade\r\nUpgrade: {upgrade}\r\n{extra}\r\n".encode())
        head, rest = receive_head(s)
        self.assertTrue(head.startswith(b"HTTP/1.1 101"), head)
        self.assertFalse(rest)
        payload = bytes([number]) * 131071
        for at in range(0, len(payload), 3071):
            part = payload[at:at+3071]
            s.sendall(part)
            self.assertEqual(receive_exact(s, len(part)), part)
        s.shutdown(socket.SHUT_WR)
        self.assertEqual(receive_exact(s, len(b"half-close-observed")), b"half-close-observed")
        self.assertEqual(s.recv(1), b"")
        s.close()

    def test_04_both_upgrade_protocols_and_halfclose(self):
        self.tunnel(True)
        self.tunnel(False)

    def test_05_concurrent_streams_and_slow_backend(self):
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            slow = pool.submit(request, "/desktop/slow")
            futures = [pool.submit(self.tunnel, bool(i % 2), i) for i in range(6)]
            time.sleep(.08)
            start = time.monotonic()
            self.assertTrue(request("/api/status").startswith(b"HTTP/1.1 200"))
            self.assertLess(time.monotonic() - start, 1.0)
            for f in futures:
                f.result(timeout=10)
            self.assertTrue(slow.result(timeout=5).startswith(b"HTTP/1.1 200"))

    def test_06_origin_smuggling_and_action_refusals(self):
        with LOCK:
            before = len(RECORDS)
        for path, method, extra in [
            ("/api/start", "POST", ""),
            ("/desktop/echo", "POST", "Origin: https://other.test\r\n"),
            ("/desktop/echo", "POST", "Transfer-Encoding: chunked\r\n"),
            ("/desktop/v1/start", "POST", "X-KVM-Box-Request: 1\r\n"),
        ]:
            self.assertNotIn(request(path, method, extra=extra).split(b" ")[1], [b"200", b"101"])
        with LOCK:
            self.assertEqual(len(RECORDS), before)

    def test_07_header_timeout(self):
        with socket.create_connection(("127.0.0.1", 8000), 4) as s:
            s.settimeout(7)
            s.sendall(b"GET / HTTP/1.1\r\nHost: fixture.test\r\n")
            head, _ = receive_head(s)
            self.assertTrue(head.startswith(b"HTTP/1.1 408"), head)

    def test_08_saturated_companion_backlog_is_bounded(self):
        self.server.shutdown()
        self.server.server_close()
        blocked = socket.socket()
        blocked.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        blocked.bind(("127.0.0.1", 19080))
        blocked.listen(1)
        fillers = []
        try:
            # Linux queues backlog + 1 completed handshakes; do not accept them.
            for _ in range(2):
                fillers.append(socket.create_connection(("127.0.0.1", 19080), 1))
            start = time.monotonic()
            response = request("/api/status")
            self.assertTrue(response.startswith(b"HTTP/1.1 502"), response)
            self.assertLess(time.monotonic() - start, 1.0)
            self.assertTrue(request("/").startswith(b"HTTP/1.1 200"))
        finally:
            for s in fillers:
                s.close()
            blocked.close()

    def test_09_idle_unauthenticated_connections_do_not_reserve_all_slots(self):
        held = []
        try:
            for _ in range(32):
                held.append(socket.create_connection(('127.0.0.1', 8000), 2))
            time.sleep(.1)
            self.assertTrue(request('/').startswith(b'HTTP/1.1 200'))
        finally:
            for s in held:
                s.close()

if __name__ == "__main__":
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(Wasip2Checks)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    receipt = {"schema": "kvm-box-controller-wasip2-check/1", "componentSha256": hashlib.sha256(Path(COMPONENT).read_bytes()).hexdigest(),
        "wasmtime": subprocess.check_output(["/usr/bin/wasmtime", "--version"], text=True).strip(),
        "testsRun": result.testsRun, "failures": len(result.failures), "errors": len(result.errors),
        "startupRefusals": getattr(Wasip2Checks, "startup_refusals", []),
        "scope": "Production Wasip2 component under local Wasmtime; fixture native companion in private network namespace. No NucBox desktop or masked GPU acceptance."}
    (HERE / "wasi-result.json").write_text(json.dumps(receipt, indent=2) + "\n")
    raise SystemExit(0 if result.wasSuccessful() else 1)
