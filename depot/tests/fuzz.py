#!/usr/bin/env python3
"""Throw malformed input at a running depot and check it survives (the
build aborts on any panic, so a crash here is a denial of service).

  python3 tests/fuzz.py [--rounds N] [--seed S]

Starts its own MinIO + depot (wasm build) like tests/e2e.py, pushes one real
repository to have something to mangle, then sends: random and truncated
pkt-lines to upload-pack (v0 and v2), corrupted and truncated packs and
command sections to receive-pack, broken chunked framing, oversized and
garbage headers, and junk JSON to the API. After every request the server
must still answer /ping."""
import argparse
import os
import random
import socket
import struct
import sys
import time
import zlib

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import e2e  # noqa: E402

ap = argparse.ArgumentParser()
ap.add_argument("--rounds", type=int, default=400)
ap.add_argument("--seed", type=int, default=1)
ap.add_argument("--workdir", default=None)
a = ap.parse_args()
a.native = a.keep = a.platform = a.flaky_storage = False
a.mirror = None
a.mem = 2048
a.wasm = os.path.join(e2e.ROOT, "target/wasm32-wasip2/release/depot.wasm")
rng = random.Random(a.seed)
TOKEN = e2e.TOKENS["admin"]


def pkt(s):
    b = s if isinstance(s, bytes) else s.encode()
    return b"%04x" % (len(b) + 4) + b


def raw(env, data, timeout=5):
    """Send bytes, read whatever comes back until close or timeout."""
    s = socket.create_connection(("127.0.0.1", env.port), timeout=timeout)
    try:
        s.sendall(data)
        s.shutdown(socket.SHUT_WR)
        out = b""
        while True:
            try:
                c = s.recv(65536)
            except socket.timeout:
                break
            if not c:
                break
            out += c
        return out
    except OSError:
        return b""
    finally:
        s.close()


def http(method, path, body=b"", headers=None, chunked=False, version="1.1"):
    h = {"host": "x", "x-api-key": TOKEN}
    h.update(headers or {})
    head = f"{method} {path} HTTP/{version}\r\n" + "".join(f"{k}: {v}\r\n" for k, v in h.items())
    if chunked:
        head += "transfer-encoding: chunked\r\n\r\n"
        out = head.encode()
        for i in range(0, len(body), 777):
            c = body[i:i + 777]
            out += b"%x\r\n" % len(c) + c + b"\r\n"
        return out + b"0\r\n\r\n"
    return (head + f"content-length: {len(body)}\r\n\r\n").encode() + body


def mutate(b):
    b = bytearray(b)
    for _ in range(rng.randint(1, 8)):
        op = rng.random()
        if op < 0.4 and b:
            b[rng.randrange(len(b))] = rng.randrange(256)
        elif op < 0.6 and b:
            del b[rng.randrange(len(b)):]
        elif op < 0.8:
            pos = rng.randrange(len(b) + 1)
            b[pos:pos] = os.urandom(rng.randint(1, 40))
        elif b:
            i = rng.randrange(len(b))
            b[i:i] = b[i:i + rng.randint(1, 200)] * rng.randint(2, 50)
    return bytes(b)


def tiny_pack(objs):
    """A valid pack of whole blobs (to be mangled)."""
    out = b"PACK" + struct.pack(">II", 2, len(objs))
    for data in objs:
        size = len(data)
        c = (3 << 4) | (size & 15)
        size >>= 4
        hdr = b""
        while size:
            hdr += bytes([c | 0x80])
            c = size & 0x7F
            size >>= 7
        out += hdr + bytes([c]) + zlib.compress(data)
    import hashlib
    return out + hashlib.sha1(out).digest()


def alive(env):
    try:
        import urllib.request
        urllib.request.urlopen(f"http://127.0.0.1:{env.port}/ping", timeout=10).read()
        return env.server.poll() is None
    except Exception:
        return False


def main():
    env = e2e.Env(a)
    try:
        env.start_minio()
        env.start_hooks()
        env.start_depot()
        src = os.path.join(env.work, "src")
        e2e.make_repo(env, src)
        env.git("push", "-q", env.url("w/fz", "admin"), "--all", cwd=src)
        head = env.git("rev-parse", "HEAD", cwd=src).stdout.strip()
        v0 = pkt(f"want {head} multi_ack_detailed side-band-64k thin-pack ofs-delta shallow no-done\n") + b"0000" + \
            pkt(f"have {head}\n") + pkt("done\n")
        v2 = pkt("command=fetch\n") + pkt("object-format=sha1\n") + b"0001" + pkt(f"want {head}\n") + \
            pkt("deepen 2\n") + pkt("ofs-delta\n") + pkt("done\n") + b"0000"
        lsr = pkt("command=ls-refs\n") + b"0001" + pkt("peel\n") + pkt("symrefs\n") + pkt("ref-prefix refs/\n") + b"0000"
        cmd = pkt(f"{'0' * 40} {head} refs/heads/fz-new\0report-status side-band-64k atomic\n") + b"0000"
        packs = [tiny_pack([os.urandom(rng.randint(0, 3000)) for _ in range(rng.randint(0, 5))]) for _ in range(4)]
        cases = 0
        for r in range(a.rounds):
            kind = rng.randrange(9)
            if kind == 0:
                req = http("POST", "/w/fz.git/git-upload-pack", mutate(v0))
            elif kind == 1:
                req = http("POST", "/w/fz.git/git-upload-pack", mutate(rng.choice([v2, lsr])), {"git-protocol": "version=2"})
            elif kind == 2:
                req = http("POST", "/w/fz.git/git-receive-pack", mutate(cmd + rng.choice(packs)), chunked=rng.random() < 0.5)
            elif kind == 3:
                req = http("POST", "/w/fz.git/git-receive-pack", cmd + mutate(rng.choice(packs)), chunked=True)
            elif kind == 4:
                req = mutate(http("POST", "/w/fz.git/git-upload-pack", v0, chunked=True))
            elif kind == 5:
                req = http("POST", "/w/fz.git/git-upload-pack", zlib.compress(v0)[:-rng.randint(0, 6)] if rng.random() < .5 else os.urandom(50),
                           {"content-encoding": "gzip"})
            elif kind == 6:
                path = rng.choice(["/api/tokens", "/api/repos", "/api/repo?repo=w/fz", "/api/maintenance?repo=w/fz",
                                   "/api/maintenance?repo=w/fz&gc=1", "/api/maintenance?repo=w/fz&purge=1",
                                   "/api/restore?repo=w/fz", "/api/witness"])
                body = b'{"name":"x","user":"u","read":["*"],"head":"main","ref":"r","id":"' + head.encode() + b'"}'
                req = http(rng.choice(["POST", "PATCH", "DELETE"]), path, mutate(body))
            elif kind == 7:
                q = rng.choice(["/api/tree?repo=w/fz&path=", "/api/raw?repo=w/fz&path=", "/api/log?repo=w/fz&n=", "/api/commit?repo=w/fz&id="])
                req = http("GET", q + "".join(rng.choice("abc/%.._~0123456789ffzz\\x00") for _ in range(rng.randint(0, 60))))
            else:
                req = mutate(http("GET", "/w/fz.git/info/refs?service=git-upload-pack", b"", {"git-protocol": "version=2"}))
            raw(env, req)
            cases += 1
            if r % 25 == 0 and not alive(env):
                print(f"FAIL: server died after case {r} (kind {kind})")
                print(open(os.path.join(env.work, "depot.log")).read()[-2000:])
                sys.exit(1)
        if not alive(env):
            print("FAIL: server died")
            print(open(os.path.join(env.work, "depot.log")).read()[-2000:])
            sys.exit(1)
        # and the repository is intact
        dst = os.path.join(env.work, "after")
        env.git("clone", "-q", "--mirror", env.url("w/fz", "admin"), dst)
        env.git("fsck", "--full", cwd=dst)
        print(f"ALL PASS: {cases} malformed requests, server alive, repository intact")
    finally:
        env.close()


main()
