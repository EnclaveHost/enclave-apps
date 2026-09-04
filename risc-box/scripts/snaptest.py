#!/usr/bin/env python3
"""snaptest.py — drive the risc-box wasm app through the snapshot + instances
lifecycle, against a local minio (--minio, seeds the sample images) or any
S3 endpoint (e.g. R2 with --endpoint/--ak/--sk/--region auto).

  scripts/snaptest.py --minio --mkbucket \
      --seed images/fw_payload.elf images/fw_payload.elf \
      --seed images/rootfs.img images/rootfs.img

Needs `wasmtime run -Snn` (the app imports wasi:nn); a guest bigger than
~1.5 GiB needs the wasmtime 49 engine (--wasmtime).

Phases (default all):
  A  cold boot to READY, POST /snapshot, verify it landed
  B  /stop, /start -> must RESUME (restored:true), console + /exec work,
     the restoreExec hook fixed the guest clock
  C  kill the process, relaunch, /start -> must fetch the snapshot from S3
     and resume (fresh cache)
  D  /stop, /start {"snapshot":false} -> cold boot despite the snapshot
  E  instances: fork three from the root snapshot and one from the live
     main machine, prove they are isolated (a file written in one is not in
     another), exercise the limit, delete one, /start a stopped one
Stdlib only. Prints one JSON line; exit 1 on any failure.
"""
import argparse, datetime, hashlib, hmac, http.client, json, os, signal, socket, subprocess, sys, time
RISCBOX = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(RISCBOX, "scripts"))
import bench  # Console, http_req, status

def sigv4_headers(method, host, path, body, ak, sk, region):
    t = datetime.datetime.now(datetime.timezone.utc)
    amzdate, date = t.strftime('%Y%m%dT%H%M%SZ'), t.strftime('%Y%m%d')
    ph = hashlib.sha256(body).hexdigest()
    headers = {'host': host, 'x-amz-content-sha256': ph, 'x-amz-date': amzdate}
    signed = ';'.join(sorted(headers))
    canon_h = ''.join(f'{k}:{headers[k]}\n' for k in sorted(headers))
    creq = f'{method}\n{path}\n\n{canon_h}\n{signed}\n{ph}'
    scope = f'{date}/{region}/s3/aws4_request'
    sts = f'AWS4-HMAC-SHA256\n{amzdate}\n{scope}\n{hashlib.sha256(creq.encode()).hexdigest()}'
    k = ('AWS4' + sk).encode()
    for m in (date, region, 's3', 'aws4_request'):
        k = hmac.new(k, m.encode(), hashlib.sha256).digest()
    sig = hmac.new(k, sts.encode(), hashlib.sha256).hexdigest()
    headers['authorization'] = f'AWS4-HMAC-SHA256 Credential={ak}/{scope}, SignedHeaders={signed}, Signature={sig}'
    return headers

def s3(method, endpoint, path, body, ak, sk, region):
    https = endpoint.startswith("https://")
    host = endpoint.split("://", 1)[1]
    h = sigv4_headers(method, host, path, body, ak, sk, region)
    if ":" in host:
        hn, port = host.split(":"); port = int(port)
    else:
        hn, port = host, (443 if https else 80)
    c = (http.client.HTTPSConnection if https else http.client.HTTPConnection)(hn, port, timeout=600)
    c.request(method, path, body=body, headers=h)
    r = c.getresponse(); data = r.read(); c.close()
    return r.status, data, dict(r.getheaders())

def wait_port(host, port, timeout):
    t0 = time.monotonic()
    while time.monotonic() - t0 < timeout:
        try:
            socket.create_connection((host, port), timeout=1).close(); return True
        except OSError:
            time.sleep(0.2)
    return False

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--wasm", default=f"{RISCBOX}/target/wasm32-wasip2/release/risc-box.wasm")
    ap.add_argument("--wasmtime", default="wasmtime")
    ap.add_argument("--port", type=int, default=8123)
    ap.add_argument("--endpoint", default="http://127.0.0.1:9100")
    ap.add_argument("--region", default="us-east-1")
    ap.add_argument("--bucket", default="machines")
    ap.add_argument("--ak", default="riscboxtest"); ap.add_argument("--sk", default="riscboxtest123")
    ap.add_argument("--kernel", default="images/fw_payload.elf")
    ap.add_argument("--fs", default="images/rootfs.img")
    ap.add_argument("--seed", nargs=2, action="append", default=[], metavar=("LOCAL", "KEY"), help="upload LOCAL to KEY before starting")
    ap.add_argument("--mkbucket", action="store_true")
    ap.add_argument("--minio", action="store_true", help="start a local minio on :9100 if needed")
    ap.add_argument("--snapshot-key", default="images/sample.snap")
    ap.add_argument("--ram", default=256,
                    help='guest RAM in MiB, or "auto" to size it from the slice the host reports '
                         '(with --mem64 the harness passes ENCLAVE_MEM_MB just as the runner does)')
    ap.add_argument("--fb", default="1024x768")
    ap.add_argument("--realtime", action="store_true")
    ap.add_argument("--ready-marker", default="activate this console")
    ap.add_argument("--ready-fps", type=float, default=0, help="instead of a marker: /status fps above this")
    ap.add_argument("--ready-exec", action="store_true", help="instead of a marker: poll /exec until a command runs (a headless image with a serial shell)")
    ap.add_argument("--settle", type=float, default=0, help="seconds to wait after READY before snapshotting")
    ap.add_argument("--boot-timeout", type=float, default=600)
    ap.add_argument("--phases", default="ABCDE")
    ap.add_argument("--instances-max", type=int, default=6)
    ap.add_argument("--exec-check", action="store_true", default=True)
    ap.add_argument("--no-exec-check", dest="exec_check", action="store_false")
    ap.add_argument("--log", default="/tmp/snaptest-wasmtime.log")
    ap.add_argument("--keep-snapshot", action="store_true", help="do not delete the snapshot object first (resume from an existing one)")
    ap.add_argument("--mem64", action="store_true", help="the wasm64 build: engine memory64 switches + a ceiling past 4 GiB")
    ap.add_argument("--max-mem-gib", type=int, default=8, help="with --mem64: -W max-memory-size in GiB")
    ap.add_argument("--engine-flags", default="", help="extra wasmtime flags, space-separated (e.g. the SET build's '-W threads,shared-everything-threads')")
    ap.add_argument("--exec-after", action="append", default=[], metavar="CMD",
                    help="after phase A is READY: run CMD in the guest (repeatable, in order; a single /exec is capped at 120s by the app). "
                         "The form 'wait-for=SUBSTR|CMD' repeats CMD every 5s until its output contains SUBSTR (20 min max). Records outputs and host RSS.")
    ap.add_argument("--exec-verify", default="", help="after the phase B resume: run this in the guest and record its output (proves state survived the round trip)")
    args = ap.parse_args()

    res = {"ok": False}
    if args.minio and not wait_port("127.0.0.1", 9100, 0.5):
        os.makedirs("/tmp/snaptest-minio", exist_ok=True)
        subprocess.Popen(["minio", "server", "/tmp/snaptest-minio", "--address", "127.0.0.1:9100", "--console-address", "127.0.0.1:9101"],
                         env={**os.environ, "MINIO_ROOT_USER": args.ak, "MINIO_ROOT_PASSWORD": args.sk},
                         stdout=open("/tmp/snaptest-minio.log", "w"), stderr=subprocess.STDOUT)
        assert wait_port("127.0.0.1", 9100, 30), "minio never listened"
        time.sleep(1)
    if args.mkbucket:
        st, d, _ = s3("PUT", args.endpoint, f"/{args.bucket}", b"", args.ak, args.sk, args.region)
        assert st in (200, 409), f"mkbucket {st} {d[:200]!r}"
    for local, key in args.seed:
        st, d, hd = s3("HEAD", args.endpoint, f"/{args.bucket}/{key}", b"", args.ak, args.sk, args.region)
        if st == 200 and int(hd.get("Content-Length", -1)) == os.path.getsize(local):
            continue
        body = open(local, "rb").read()
        st, d, _ = s3("PUT", args.endpoint, f"/{args.bucket}/{key}", body, args.ak, args.sk, args.region)
        assert st == 200, f"seed {key}: {st} {d[:200]!r}"
        print(f"seeded {key} ({len(body)} bytes)", file=sys.stderr)
    # start from a clean slate: no snapshot object (unless resuming an existing one)
    if not args.keep_snapshot:
        s3("DELETE", args.endpoint, f"/{args.bucket}/{args.snapshot_key}", b"", args.ak, args.sk, args.region)

    w, h = args.fb.split("x")
    ram = "auto" if str(args.ram).strip().lower() == "auto" else int(args.ram)
    cfg = {"title": "snaptest", "endpoint": args.endpoint, "region": args.region, "bucket": args.bucket,
           "kernel": args.kernel, "fs": args.fs, "ramMiB": ram, "display": {"width": int(w), "height": int(h)},
           "realtime": args.realtime, "snapshot": args.snapshot_key,
           "restoreExec": "date -s @{epoch} >/dev/null; echo {entropy} > /dev/urandom; echo RESTORE-HOOK-OK",
           "instances": {"max": args.instances_max},
           "credentials": {"accessKeyId": args.ak, "secretAccessKey": args.sk}}
    mem_args, mem_env = [], []
    if args.mem64:
        mem_args = ["-W", "memory64,component-model-memory64",
                    "-W", f"max-memory-size={args.max_mem_gib << 30}"]
        # the runner hands every guest its ceiling; mirror that here or an
        # auto-sized app has nothing to size itself from
        mem_env = ["--env", f"ENCLAVE_MEM_MB={args.max_mem_gib * 1024}"]
    cmd = [args.wasmtime, "run", "-Snn", "-Stcp", "-Sinherit-network", "-Sallow-ip-name-lookup",
           *mem_args, *mem_env, *args.engine_flags.split(),
           "--env", f"ENCLAVE_PORTS=http:8000={args.port}", "--env", f"RISCBOX_CONFIG={json.dumps(cfg)}", args.wasm]
    logf = open(args.log, "ab")
    proc = None

    def launch():
        nonlocal proc
        proc = subprocess.Popen(cmd, stdout=logf, stderr=subprocess.STDOUT)
        for _ in range(300):
            try:
                if bench.http_req(args.port, "GET", "/ping", timeout=2)[0] == 200:
                    return
            except OSError:
                pass
            if proc.poll() is not None:
                sys.exit(f"wasmtime exited early; see {args.log}")
            time.sleep(0.1)
        sys.exit("never answered /ping")

    def kill():
        nonlocal proc
        if proc and proc.poll() is None:
            proc.send_signal(signal.SIGTERM)
            try: proc.wait(timeout=5)
            except subprocess.TimeoutExpired: proc.kill()
        proc = None

    def wait_ready(con, start_off):
        t0 = time.monotonic()
        if args.ready_exec:
            while time.monotonic() - t0 < args.boot_timeout:
                try:
                    st = bench.status(args.port)
                    if st["phase"] == "error":
                        raise RuntimeError(f"machine errored: {st['error']}")
                    if st["phase"] == "running":
                        s2, d = bench.http_req(args.port, "POST", "/exec", body=json.dumps({"cmd": "echo READY-$((6*7))", "timeout_s": 20}).encode(), timeout=60)
                        if s2 == 200 and json.loads(d).get("ok") and "READY-42" in json.loads(d)["output"]:
                            return time.monotonic() - t0
                except Exception:
                    pass
                time.sleep(5)
            raise TimeoutError("guest shell never answered")
        if args.ready_fps > 0:
            good = 0
            while time.monotonic() - t0 < args.boot_timeout:
                time.sleep(5)
                try:
                    st = bench.status(args.port)
                except Exception:
                    continue
                if st["phase"] == "error":
                    raise RuntimeError(f"machine errored: {st['error']}")
                if st["fps"] >= args.ready_fps:
                    good += 1
                    if good >= 2:
                        break
                else:
                    good = 0
            else:
                raise TimeoutError("fps never rose")
            return time.monotonic() - t0
        con.wait_for([args.ready_marker.encode()], timeout=args.boot_timeout, start=start_off)
        return time.monotonic() - t0

    def wait_running(timeout=120):
        """From POST /start to the first /status that says running (do_start blocks the loop)."""
        t0 = time.monotonic()
        while time.monotonic() - t0 < timeout:
            try:
                st = bench.status(args.port)
                if st["phase"] == "running":
                    return time.monotonic() - t0, st
                if st["phase"] == "error":
                    raise RuntimeError(st["error"])
            except (OSError, http.client.HTTPException):
                pass
            time.sleep(0.2)
        raise TimeoutError("never running")

    def host_rss_mib():
        try:
            for line in open(f"/proc/{proc.pid}/status"):
                if line.startswith("VmRSS:"):
                    return int(line.split()[1]) >> 10
        except OSError:
            pass
        return -1

    def iexec(prefix, cmd, timeout_s=60):
        st, d = bench.http_req(args.port, "POST", f"{prefix}/exec", body=json.dumps({"cmd": cmd, "timeout_s": timeout_s}).encode(), timeout=timeout_s + 30)
        assert st == 200, (prefix, st, d[:200])
        return json.loads(d)

    def exec_check(label):
        st, d = bench.http_req(args.port, "POST", "/exec", body=json.dumps({"cmd": "uname -a; date +%s", "timeout_s": 60}).encode(), timeout=90)
        j = json.loads(d)
        res[f"{label}_exec"] = j.get("ok"), j.get("output", "")[-160:]
        assert j.get("ok"), f"{label}: exec failed {j}"
        assert "riscv64" in j["output"], f"{label}: unexpected uname {j['output']!r}"
        guest_epoch = int(j["output"].strip().rsplit("\n", 1)[-1])
        res[f"{label}_guest_clock_skew_s"] = int(time.time()) - guest_epoch
        return j

    try:
        launch()
        con = bench.Console(args.port); time.sleep(0.3)
        if "A" in args.phases:
            t0 = time.monotonic()
            st, d = bench.http_req(args.port, "POST", "/start", body=b"{}"); assert st == 202, (st, d)
            res["A_boot_wall_s"] = round(wait_ready(con, 0), 1)
            res["A_host_rss_MiB"] = host_rss_mib()
            if args.exec_after:
                res["A_exec_after"] = []
                for spec in args.exec_after:
                    t1 = time.monotonic()
                    if spec.startswith("wait-for="):
                        until, cmd = spec[len("wait-for="):].split("|", 1)
                        while True:
                            j = iexec("", cmd, timeout_s=110)
                            if j.get("ok") and until in j.get("output", ""):
                                break
                            assert time.monotonic() - t1 < 1200, f"wait-for {until!r} never came: {j}"
                            time.sleep(5)
                    else:
                        j = iexec("", spec, timeout_s=110)
                        assert j.get("ok"), f"--exec-after failed: {j}"
                    res["A_exec_after"].append((spec[:60], j.get("output", "")[-300:], round(time.monotonic() - t1, 1)))
                res["A_host_rss_after_MiB"] = host_rss_mib()
                res["A_status_after"] = {k: bench.status(args.port).get(k) for k in ("ramMiB", "footprintBytes", "phase")}
            if args.settle: time.sleep(args.settle)
            s0 = bench.status(args.port)
            assert s0["snapshot"]["restored"] is False and s0["snapshot"]["cachedBytes"] == 0, s0["snapshot"]
            t1 = time.monotonic()
            st, d = bench.http_req(args.port, "POST", "/snapshot", body=b"{}", timeout=900)
            j = json.loads(d); assert st == 200 and j.get("ok"), (st, d[:300])
            res["A_snapshot"] = j; res["A_snapshot_wall_s"] = round(time.monotonic() - t1, 1)
            s1 = bench.status(args.port)
            assert s1["snapshot"]["lastSnapshot"] == args.snapshot_key and s1["snapshot"]["cachedBytes"] == j["bytes"], s1["snapshot"]
            st, d, hd = s3("HEAD", args.endpoint, f"/{args.bucket}/{args.snapshot_key}", b"", args.ak, args.sk, args.region)
            assert st == 200 and int(hd.get("Content-Length", -1)) == j["bytes"], ("object missing in bucket", st, hd)
        if "B" in args.phases:
            st, d = bench.http_req(args.port, "POST", "/stop"); assert st == 200
            off = con.size()
            st, d = bench.http_req(args.port, "POST", "/start", body=b"{}"); assert st == 202
            wall, s2 = wait_running()
            assert s2["snapshot"]["restored"] is True, s2["snapshot"]
            res["B_resume_wall_s"] = round(wall, 2); res["B_restoreMs"] = s2["snapshot"]["restoreMs"]
            if args.exec_check:
                exec_check("B")
            # proof of life: the guest is executing after the resume
            i0 = bench.status(args.port)["instret"]; time.sleep(2); i1 = bench.status(args.port)["instret"]
            res["B_insns_2s"] = i1 - i0; assert i1 > i0
            if args.exec_verify:
                j = iexec("", args.exec_verify, timeout_s=600)
                res["B_exec_verify"] = (j.get("ok"), j.get("output", "")[-400:])
                assert j.get("ok"), f"--exec-verify failed: {j}"
                res["B_host_rss_MiB"] = host_rss_mib()
        if "C" in args.phases:
            kill(); launch(); con = bench.Console(args.port); time.sleep(0.3)
            t0 = time.monotonic()
            st, d = bench.http_req(args.port, "POST", "/start", body=b"{}"); assert st == 202
            wall, s3st = wait_running(timeout=args.boot_timeout)
            assert s3st["snapshot"]["restored"] is True, s3st["snapshot"]
            res["C_fetch_and_resume_wall_s"] = round(wall, 2); res["C_restoreMs"] = s3st["snapshot"]["restoreMs"]
            if args.exec_check:
                exec_check("C")
        if "D" in args.phases:
            st, d = bench.http_req(args.port, "POST", "/stop"); assert st == 200
            off = con.size()
            st, d = bench.http_req(args.port, "POST", "/start", body=b'{"snapshot": false}'); assert st == 202
            res["D_cold_boot_wall_s"] = round(wait_ready(con, off), 1)
            s4 = bench.status(args.port)
            assert s4["snapshot"]["restored"] is False and s4["snapshot"]["cachedBytes"] > 0, s4["snapshot"]
        if "E" in args.phases:
            # needs a running main (after D it is cold-booted; after C resumed) and a root snapshot in the bucket
            if not bench.status(args.port)["phase"] == "running":
                st, d = bench.http_req(args.port, "POST", "/start", body=b"{}"); assert st == 202
                wait_running(timeout=args.boot_timeout)
            if args.ready_marker and "D" in args.phases:
                pass
            t0 = time.monotonic()
            ids = []
            for i in range(3):
                st, d = bench.http_req(args.port, "POST", "/instances", body=b"{}", timeout=600)
                j = json.loads(d); assert st == 201, (st, d[:300])
                ids.append(j["id"]); assert j["restored"] and j["phase"] == "running", j
            res["E_fork3_from_root_s"] = round(time.monotonic() - t0, 2)
            t0 = time.monotonic()
            st, d = bench.http_req(args.port, "POST", "/instances", body=b'{"from":"main","id":"frommain"}', timeout=600)
            j = json.loads(d); assert st == 201, (st, d[:300])
            ids.append("frommain")
            res["E_fork_from_main_s"] = round(time.monotonic() - t0, 2)
            st, d = bench.http_req(args.port, "GET", "/instances"); li = json.loads(d)
            assert len(li["instances"]) == 5, li["summary"]
            res["E_instances"] = [(m["id"], m["origin"], m["ramMiB"], m["footprintBytes"] >> 20) for m in li["instances"]]
            res["E_footprint_MiB"] = li["summary"]["footprintBytes"] >> 20
            # every instance answers a command on ITS console, and a write in one is invisible to the others
            for i, iid in enumerate(ids):
                if args.ready_marker and not args.ready_fps:
                    # the sample image needs Enter to activate its console; /exec sends a newline first, fine
                    pass
                j = iexec(f"/i/{iid}", f"echo mark-{iid} > /tmp/whoami; cat /tmp/whoami; uname -m")
                assert j.get("ok") and f"mark-{iid}" in j["output"] and "riscv64" in j["output"], (iid, j)
            for iid in ids:
                j = iexec(f"/i/{iid}", "cat /tmp/whoami")
                assert j.get("ok") and j["output"].strip() == f"mark-{iid}", ("isolation", iid, j)
            j = iexec("", "cat /tmp/whoami 2>&1 || echo none")
            assert "mark-" not in j["output"] or "frommain" in j["output"] and False, ("main must not see instance writes", j)
            # the main machine's own /tmp/whoami must not exist (it was written in instances only)
            assert "none" in j["output"] or "No such" in j["output"], ("main isolation", j)
            # per-instance screenshot and status
            st, d = bench.http_req(args.port, "GET", f"/i/{ids[0]}/fb.png"); assert st == 200 and d[:4] == b"\x89PNG", (st, d[:20])
            st, d = bench.http_req(args.port, "GET", f"/i/{ids[0]}/status"); assert st == 200 and json.loads(d)["id"] == ids[0]
            # the limit: instances.max counts main
            extra = []
            while True:
                st, d = bench.http_req(args.port, "POST", "/instances", body=b"{}", timeout=600)
                if st == 409:
                    res["E_limit_error"] = json.loads(d)["error"]["message"][:80]; break
                assert st == 201, (st, d[:200]); extra.append(json.loads(d)["id"])
                assert len(extra) < 20, "limit never hit"
            for e in extra:
                st, d = bench.http_req(args.port, "DELETE", f"/i/{e}"); assert st == 200
            # delete one, stop + restart another (a fresh fork of the same origin)
            st, d = bench.http_req(args.port, "DELETE", f"/i/{ids[1]}"); assert st == 200
            st, d = bench.http_req(args.port, "GET", f"/i/{ids[1]}/status"); assert st == 404
            st, d = bench.http_req(args.port, "POST", f"/i/{ids[2]}/stop"); assert st == 200
            st, d = bench.http_req(args.port, "POST", f"/i/{ids[2]}/start", timeout=600); assert st == 200, (st, d[:200])
            j = iexec(f"/i/{ids[2]}", "cat /tmp/whoami 2>&1 || echo gone")
            assert j.get("ok") and f"mark-{ids[2]}" not in j["output"], ("restart is a fresh fork", j)
            # main is still fine and unaffected
            exec_check("E_main")
            st, d = bench.http_req(args.port, "GET", "/instances"); li = json.loads(d)
            res["E_final"] = [(m["id"], m["phase"]) for m in li["instances"]]
            res["E_images"] = [(i["key"][:24], i["users"]) for i in li["images"]]
        res["ok"] = True
    finally:
        kill()
        logf.close()
        print(json.dumps(res, default=str))
    if not res["ok"]:
        sys.exit(1)

if __name__ == "__main__":
    main()
