#!/usr/bin/env python3
"""bench.py — spawn risc-box under wasmtime against local minio, drive it
end-to-end, and measure. Stdlib only.

Measures:
  boot_wall      seconds from POST /start (202) to login banner in console
  boot_instr     instructions retired at that moment
  b2_wall/b2_mips  fixed-work shell loop (50k iterations) wall time + MIPS
  idle_mips/idle_cpu  instret rate + host CPU% while guest sits at prompt
  save_ok        (--save) POST /save then SigV4 ranged GET verifies size

Exit 0 with a JSON line on success; nonzero + diagnostics on any failure.
"""
import argparse, base64, datetime, hashlib, hmac, http.client, json, os, re
import signal, socket, subprocess, sys, threading, time

MINIO = "http://127.0.0.1:9100"
REGION = "us-east-1"
BUCKET = "machines"
AK, SK = "riscboxtest", "riscboxtest123"
SAVEKEY = "images/rootfs.saved.img"
ROOTFS_SIZE = 52428800


# ---- console collector: raw socket, de-chunk, SSE data: lines -> bytes ------
class Console:
    def __init__(self, port):
        self.buf = bytearray()
        self.lock = threading.Lock()
        self.dead = None
        self.port = port
        self.t = threading.Thread(target=self._run, daemon=True)
        self.t.start()

    def _run(self):
        try:
            s = socket.create_connection(("127.0.0.1", self.port), timeout=10)
            s.sendall(b"GET /console HTTP/1.1\r\nhost: x\r\naccept: text/event-stream\r\n\r\n")
            s.settimeout(600)
            raw = bytearray()
            # headers
            while b"\r\n\r\n" not in raw:
                raw += s.recv(4096)
            head, rest = bytes(raw).split(b"\r\n\r\n", 1)
            if b"200" not in head.split(b"\r\n")[0]:
                self.dead = f"console HTTP: {head.split(b'\r\n')[0]!r}"
                return
            chunked = b"chunked" in head.lower()
            payload = bytearray()
            buf = bytearray(rest)

            def feed(data):
                payload.extend(data)
                while b"\n" in payload:
                    line, _, r2 = bytes(payload).partition(b"\n")
                    del payload[: len(line) + 1]
                    line = line.strip(b"\r")
                    if line.startswith(b"data: "):
                        try:
                            chunk = base64.b64decode(line[6:])
                        except Exception:
                            continue
                        with self.lock:
                            self.buf.extend(chunk)

            if not chunked:
                feed(buf)
                while True:
                    d = s.recv(65536)
                    if not d:
                        break
                    feed(d)
            else:
                # de-chunk
                while True:
                    while b"\r\n" not in buf:
                        d = s.recv(65536)
                        if not d:
                            return
                        buf += d
                    szline, _, r = bytes(buf).partition(b"\r\n")
                    sz = int(szline.split(b";")[0], 16)
                    buf = bytearray(r)
                    if sz == 0:
                        return
                    while len(buf) < sz + 2:
                        d = s.recv(65536)
                        if not d:
                            return
                        buf += d
                    feed(bytes(buf[:sz]))
                    del buf[: sz + 2]
        except Exception as e:
            self.dead = f"console reader died: {e}"

    def wait_for(self, patterns, timeout, start=0):
        """Return (pattern, end_offset) for first of `patterns` found at >=start."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            with self.lock:
                data = bytes(self.buf[start:])
            for p in patterns:
                i = data.find(p)
                if i >= 0:
                    return p, start + i + len(p)
            if self.dead:
                raise RuntimeError(self.dead)
            time.sleep(0.05)
        with self.lock:
            tail = bytes(self.buf[-600:])
        raise TimeoutError(f"waiting for {patterns}; console tail: {tail!r}")

    def size(self):
        with self.lock:
            return len(self.buf)


def http_req(port, method, path, body=None, timeout=120):
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    c.request(method, path, body=body)
    r = c.getresponse()
    data = r.read()
    c.close()
    return r.status, data


def status(port):
    st, data = http_req(port, "GET", "/status")
    assert st == 200, f"/status -> {st}"
    return json.loads(data)


def proc_cpu_seconds(pid):
    with open(f"/proc/{pid}/stat") as f:
        parts = f.read().rsplit(")", 1)[1].split()
    return (int(parts[11]) + int(parts[12])) / os.sysconf("SC_CLK_TCK")


def _s3_get(key, range_header=None):
    """SigV4 GET; returns the http response object (unread)."""
    now = datetime.datetime.now(datetime.timezone.utc)
    stamp, date = now.strftime("%Y%m%dT%H%M%SZ"), now.strftime("%Y%m%d")
    uri = f"/{BUCKET}/{key}"
    payload_hash = hashlib.sha256(b"").hexdigest()
    headers = {"host": "127.0.0.1:9100", "x-amz-content-sha256": payload_hash, "x-amz-date": stamp}
    if range_header:
        headers["range"] = range_header
    signed = ";".join(sorted(headers))
    canon = "".join(f"{k}:{headers[k]}\n" for k in sorted(headers))
    creq = f"GET\n{uri}\n\n{canon}\n{signed}\n{payload_hash}"
    scope = f"{date}/{REGION}/s3/aws4_request"
    sts = f"AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{hashlib.sha256(creq.encode()).hexdigest()}"
    k = ("AWS4" + SK).encode()
    for m in (date, REGION, "s3", "aws4_request"):
        k = hmac.new(k, m.encode(), hashlib.sha256).digest()
    sig = hmac.new(k, sts.encode(), hashlib.sha256).hexdigest()
    headers["authorization"] = (
        f"AWS4-HMAC-SHA256 Credential={AK}/{scope}, SignedHeaders={signed}, Signature={sig}"
    )
    c = http.client.HTTPConnection("127.0.0.1", 9100, timeout=120)
    c.request("GET", uri, headers=headers)
    return c.getresponse()


def s3_object_contains(key, needle):
    """Full GET, streamed; True if needle occurs anywhere in the object."""
    r = _s3_get(key)
    if r.status != 200:
        r.read()
        return False
    tail = b""
    while True:
        chunk = r.read(1 << 20)
        if not chunk:
            return False
        if needle in tail + chunk:
            return True
        tail = chunk[-(len(needle) - 1):]


def s3_total_size(key):
    """SigV4 ranged GET (bytes=0-0); returns total size from Content-Range."""
    now = datetime.datetime.now(datetime.timezone.utc)
    stamp, date = now.strftime("%Y%m%dT%H%M%SZ"), now.strftime("%Y%m%d")
    uri = f"/{BUCKET}/{key}"
    payload_hash = hashlib.sha256(b"").hexdigest()
    headers = {
        "host": "127.0.0.1:9100",
        "range": "bytes=0-0",
        "x-amz-content-sha256": payload_hash,
        "x-amz-date": stamp,
    }
    signed = ";".join(sorted(headers))
    canon = "".join(f"{k}:{headers[k]}\n" for k in sorted(headers))
    creq = f"GET\n{uri}\n\n{canon}\n{signed}\n{payload_hash}"
    scope = f"{date}/{REGION}/s3/aws4_request"
    sts = f"AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{hashlib.sha256(creq.encode()).hexdigest()}"
    k = ("AWS4" + SK).encode()
    for m in (date, REGION, "s3", "aws4_request"):
        k = hmac.new(k, m.encode(), hashlib.sha256).digest()
    sig = hmac.new(k, sts.encode(), hashlib.sha256).hexdigest()
    headers["authorization"] = (
        f"AWS4-HMAC-SHA256 Credential={AK}/{scope}, SignedHeaders={signed}, Signature={sig}"
    )
    c = http.client.HTTPConnection("127.0.0.1", 9100, timeout=30)
    c.request("GET", uri, headers=headers)
    r = c.getresponse()
    r.read()
    c.close()
    if r.status != 206:
        return None
    cr = r.getheader("Content-Range", "")
    m = re.match(r"bytes \d+-\d+/(\d+)", cr)
    return int(m.group(1)) if m else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--wasm", default="/home/steven/Projects/enclave-apps/risc-box/target/wasm32-wasip2/release/risc-box.wasm")
    ap.add_argument("--port", type=int, default=8000)
    ap.add_argument("--label", default="run")
    ap.add_argument("--save", action="store_true", help="also test POST /save + verify in bucket")
    ap.add_argument("--deep", action="store_true", help="guest file write + idle soak + saved-content check")
    ap.add_argument("--env", action="append", default=[], help="extra KEY=VAL env for wasmtime")
    ap.add_argument("--iters", type=int, default=50000, help="B2 loop iterations")
    ap.add_argument("--idle-secs", type=float, default=5.0)
    args = ap.parse_args()

    cfg = json.dumps({
        "title": "bench", "endpoint": MINIO, "region": REGION, "bucket": BUCKET,
        "kernel": "images/fw_payload.elf", "fs": "images/rootfs.img",
        "saveKey": SAVEKEY,
        "credentials": {"accessKeyId": AK, "secretAccessKey": SK},
    })
    cmd = ["wasmtime", "run", "-Stcp", "-Sinherit-network", "-Sallow-ip-name-lookup",
           "--env", f"ENCLAVE_PORTS=http:8000={args.port}",
           "--env", f"RISCBOX_CONFIG={cfg}"]
    for kv in args.env:
        cmd += ["--env", kv]
    cmd.append(args.wasm)

    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    res = {"label": args.label, "ok": False}
    try:
        # wait for /ping
        for _ in range(100):
            try:
                st, _d = http_req(args.port, "GET", "/ping", timeout=2)
                if st == 200:
                    break
            except OSError:
                pass
            if proc.poll() is not None:
                out = proc.stdout.read().decode(errors="replace")
                sys.exit(f"wasmtime exited early:\n{out[-2000:]}")
            time.sleep(0.1)
        else:
            sys.exit("risc-box never answered /ping")

        con = Console(args.port)
        time.sleep(0.3)

        t0 = time.monotonic()
        st, d = http_req(args.port, "POST", "/start", body=b"{}")
        assert st == 202, f"/start -> {st} {d!r}"

        pat, off = con.wait_for([b"login:", b"activate this console"], timeout=180)
        res["boot_wall"] = round(time.monotonic() - t0, 2)
        res["boot_instr"] = status(args.port)["instret"]

        http_req(args.port, "POST", "/input", body=b"root\n" if pat == b"login:" else b"\n")
        _, off = con.wait_for([b"# "], timeout=60, start=off)

        # settle, then idle measurement
        time.sleep(1.0)
        i0, c0, t0i = status(args.port)["instret"], proc_cpu_seconds(proc.pid), time.monotonic()
        time.sleep(args.idle_secs)
        i1, c1, t1i = status(args.port)["instret"], proc_cpu_seconds(proc.pid), time.monotonic()
        res["idle_mips"] = round((i1 - i0) / (t1i - t0i) / 1e6, 1)
        res["idle_cpu"] = round((c1 - c0) / (t1i - t0i) * 100, 1)

        # B2: fixed-work busy loop; completion marker computed by the guest so
        # the echoed command line can't false-match
        mark = b"B2-42-END"
        cmdline = (f"i=0; while [ $i -lt {args.iters} ]; do i=$((i+1)); done; "
                   f"echo B2-$((40+2))-END\n").encode()
        pre = status(args.port)["instret"]
        t0b = time.monotonic()
        off0 = con.size()
        http_req(args.port, "POST", "/input", body=cmdline)
        _, off = con.wait_for([mark], timeout=600, start=off0)
        t1b = time.monotonic()
        post = status(args.port)["instret"]
        res["b2_wall"] = round(t1b - t0b, 2)
        res["b2_instr"] = post - pre
        res["b2_mips"] = round((post - pre) / (t1b - t0b) / 1e6, 1)

        # interactive round-trip canary
        off0 = con.size()
        http_req(args.port, "POST", "/input", body=b"echo rt-$((6*7))\n")
        con.wait_for([b"rt-42"], timeout=60, start=off0)
        res["roundtrip"] = True

        if args.deep:
            # guest writes a file (exercises store-path translation + PTE
            # D bits); after save we must find its content in the image
            off0 = con.size()
            http_req(args.port, "POST", "/input",
                     body=b"echo riscbox-$((100+23))-proof > /probe.txt; cat /probe.txt\n")
            con.wait_for([b"riscbox-123-proof"], timeout=120, start=off0)
            res["guest_write"] = True

            # write-then-execute: fresh pages holding new code must run
            # correctly (exercises the SMC snoop / predecode invalidation)
            off0 = con.size()
            http_req(args.port, "POST", "/input",
                     body=b'echo "echo smc-$((7*6))-ok" > /t.sh; sh /t.sh\n')
            con.wait_for([b"smc-42-ok"], timeout=120, start=off0)
            res["smc_exec"] = True

            # 30s idle soak, then prove the machine still wakes and responds
            time.sleep(30)
            off0 = con.size()
            http_req(args.port, "POST", "/input", body=b"echo wake-$((50*2))\n")
            con.wait_for([b"wake-100"], timeout=60, start=off0)
            res["idle_soak_wake"] = True

        if args.save:
            off0 = con.size()
            http_req(args.port, "POST", "/input", body=b"sync\n")
            con.wait_for([b"# "], timeout=60, start=off0)
            st, d = http_req(args.port, "POST", "/save", timeout=300)
            res["save_status"] = st
            sz = s3_total_size(SAVEKEY)
            res["save_size"] = sz
            res["save_ok"] = (st == 200 and sz == ROOTFS_SIZE)
            if args.deep and res["save_ok"]:
                res["saved_content_ok"] = s3_object_contains(SAVEKEY, b"riscbox-123-proof")

        res["ok"] = True
    finally:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
        print(json.dumps(res))

    if not res["ok"]:
        sys.exit(1)


if __name__ == "__main__":
    main()
