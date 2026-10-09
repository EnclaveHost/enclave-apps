#!/usr/bin/env python3
"""End-to-end test of depot: the real component (or the native build) under
wasmtime, a local MinIO as the bucket, and the real git client.

  python3 tests/e2e.py                 # wasm build, all checks
  python3 tests/e2e.py --native        # target/release/depot instead
  python3 tests/e2e.py --mirror ~/src/some-repo   # also push+clone a real repository

Needs: minio, wasmtime, git, curl. Everything binds to loopback; nothing
leaves the machine. Exits non-zero on the first failed check.
"""
import argparse
import base64
import hashlib
import json
import os
import random
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AK, SK = "depotaccesskey01", "depotsecretkey0123456789"
BUCKET = "depot-e2e"
TOKENS = {
    "admin": "adm-" + "a1" * 16,
    "reader": "rdr-" + "b2" * 16,
    "writer": "wtr-" + "c3" * 16,
}
MARK = "PLAINTEXT-MARKER-7f3a9c"
HOOK_SECRET = "hook-secret-0123456789abcdef"
# Sign in with Enclave: the platform spec's throwaway signer key 0x42..42 and its address
SSO_SIGNER = "0x17c5185167401ed00cf5f5b2fc97d9bbfdb7d025"
SSO_AUD = "0x" + "11" * 32
SSO_SUB = "0x00a329c0648769a73afac7f9381e08fb43dbea72"


def mint_est1(sub, aud, iat, exp, key="0x" + "42" * 32):
    """An EST1 sign-in token, minted as the platform mints them (cast signs)."""
    b64 = lambda b: base64.urlsafe_b64encode(b).rstrip(b"=").decode()
    msg = "EST1." + b64(json.dumps({"v": 1, "sub": sub, "aud": aud, "iat": iat, "exp": exp}, separators=(",", ":")).encode())
    sig = subprocess.check_output(["cast", "wallet", "sign", "--private-key", key, msg]).decode().strip()
    return msg + "." + b64(bytes.fromhex(sig[2:]))  # must never appear in the bucket

passed = 0


def ok(msg):
    global passed
    passed += 1
    print(f"  ok  {msg}")


def die(msg):
    print(f"FAIL  {msg}")
    sys.exit(1)


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


class Env:
    def __init__(self, args):
        self.args = args
        self.work = tempfile.mkdtemp(prefix="depot-e2e-", dir=args.workdir)
        self.home = os.path.join(self.work, "home")
        os.makedirs(self.home)
        self.procs = []
        self.server = None
        self.git_env = dict(
            os.environ,
            HOME=self.home,
            GIT_CONFIG_NOSYSTEM="1",
            GIT_TERMINAL_PROMPT="0",
            GIT_AUTHOR_NAME="E2E Author",
            GIT_AUTHOR_EMAIL="author@example.invalid",
            GIT_COMMITTER_NAME="E2E Committer",
            GIT_COMMITTER_EMAIL="committer@example.invalid",
            LC_ALL="C",
        )
        self.git_env.pop("GIT_DIR", None)

    def close(self):
        for p in self.procs + ([self.server] if self.server else []):
            if p and p.poll() is None:
                p.terminate()
                try:
                    p.wait(5)
                except subprocess.TimeoutExpired:
                    p.kill()
        if not self.args.keep:
            shutil.rmtree(self.work, ignore_errors=True)
        else:
            print(f"kept {self.work}")

    # ---- minio -----------------------------------------------------------
    def start_minio(self):
        self.s3_port = free_port()
        self.minio_dir = os.path.join(self.work, "minio")
        os.makedirs(self.minio_dir)
        env = dict(os.environ, MINIO_ROOT_USER=AK, MINIO_ROOT_PASSWORD=SK, MINIO_BROWSER="off")
        log = open(os.path.join(self.work, "minio.log"), "w")
        p = subprocess.Popen(
            ["minio", "server", "--quiet", "--address", f"127.0.0.1:{self.s3_port}", self.minio_dir],
            env=env, stdout=log, stderr=log,
        )
        self.procs.append(p)
        self.s3 = f"http://127.0.0.1:{self.s3_port}"
        for _ in range(100):
            try:
                urllib.request.urlopen(f"{self.s3}/minio/health/ready", timeout=1)
                break
            except Exception:
                time.sleep(0.1)
        else:
            die("minio did not come up")
        self.s3curl("PUT", f"/{BUCKET}")

    def s3curl(self, method, path, out=None):
        cmd = ["curl", "-s", "-X", method, "--aws-sigv4", "aws:amz:us-east-1:s3", "--user", f"{AK}:{SK}",
               "-o", out or "/dev/null", "-w", "%{http_code}", f"{self.s3}{path}"]
        return subprocess.run(cmd, capture_output=True, text=True).stdout

    def bucket_objects(self):
        out = os.path.join(self.work, "list.xml")
        self.s3curl("GET", f"/{BUCKET}?list-type=2", out)
        x = open(out).read()
        keys = []
        for part in x.split("<Key>")[1:]:
            keys.append(part.split("</Key>")[0])
        return keys

    # ---- webhook receiver -------------------------------------------------
    def start_hooks(self):
        import http.server
        import threading
        env = self
        self.deliveries = []

        class H(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                body = self.rfile.read(int(self.headers["content-length"]))
                env.deliveries.append(({k.lower(): v for k, v in self.headers.items()}, body))
                self.send_response(204)
                self.end_headers()

            def log_message(self, *a):
                pass

        self.hook_port = free_port()
        srv = http.server.ThreadingHTTPServer(("127.0.0.1", self.hook_port), H)
        threading.Thread(target=srv.serve_forever, daemon=True).start()

    # ---- depot -----------------------------------------------------------
    def config(self, extra=None):
        c = {
            "storage": {"endpoint": self.s3, "region": "us-east-1", "bucket": BUCKET, "prefix": "depot/",
                        "access_key": "$E2E_AK", "secret_key": "$E2E_SK"},
            "master_key": "$E2E_MASTER",
            "local_test": True,
            "users": {
                "admin": {"token": "$E2E_TOK_ADMIN", "admin": True},
                "reader": {"token_sha256": hashlib.sha256(TOKENS["reader"].encode()).hexdigest(), "read": ["*"]},
                "writer": {"token": "$E2E_TOK_WRITER", "write": ["w/*", "shared"]},
                "signed": {"account": SSO_SUB, "read": ["w/*"]},
            },
            "sso": {"signer": SSO_SIGNER, "audience": SSO_AUD},
            "public": ["pub/*"],
            "protected": ["refs/heads/main", "refs/tags/*"],
            "max_push_mb": 512,
            "cache_mb": 64,
            "title": "depot e2e",
            "hooks": [{"url": f"http://127.0.0.1:{self.hook_port}/hook", "secret": "$E2E_HOOK", "repos": ["w/*"]}],
        }
        if extra:
            c.update(extra)
        return json.dumps(c)

    def start_depot(self, extra=None):
        self.port = free_port()
        env = dict(
            os.environ,
            ENCLAVE_CONFIG=self.config(extra),
            ENCLAVE_PORTS=f"http:8000={self.port}",
            E2E_AK=AK,
            E2E_SK=SK,
            E2E_MASTER="m" * 40,
            E2E_TOK_ADMIN=TOKENS["admin"],
            E2E_TOK_WRITER=TOKENS["writer"],
            E2E_HOOK=HOOK_SECRET,
            DEPOT_RETIRE_GRACE="2",
            DEPOT_SWEEP_EVERY="1",
            DEPOT_ORPHAN_AGE="5",
        )
        if self.args.platform:
            if not getattr(self, "socks_port", None):
                self.socks_port = free_port()
                self.socks_count = os.path.join(self.work, "socks.count")
                self.procs.append(subprocess.Popen(
                    [sys.executable, os.path.join(ROOT, "tests/socks5.py"), str(self.socks_port), "dep-id", "egress-token",
                     self.socks_count]))
                time.sleep(0.3)
            env["ENCLAVE_EGRESS"] = f"socks5://dep-id:egress-token@127.0.0.1:{self.socks_port}"
        secret_envs = ["E2E_AK", "E2E_SK", "E2E_MASTER", "E2E_TOK_ADMIN", "E2E_TOK_WRITER", "E2E_HOOK"]
        dirs = []
        if self.args.platform and not self.args.native:
            # as the platform's manager does it: secrets substituted into the
            # config text, which arrives as ENCLAVE_CONFIG and as a read-only
            # file at /config; the guest sees no secret variables at all
            text = env["ENCLAVE_CONFIG"]
            for k in secret_envs:
                text = text.replace("$" + k, env[k])
            env["ENCLAVE_CONFIG"] = text
            cfgdir = os.path.join(self.work, "cfg")
            os.makedirs(cfgdir, exist_ok=True)
            open(os.path.join(cfgdir, "config.json"), "w").write(text)
            env["ENCLAVE_CONFIG_FILE"] = "/config/config.json"
            env["ENCLAVE_MEM_MB"] = str(self.args.mem)
            dirs = ["--dir", f"{cfgdir}::/config"]
            secret_envs = ["ENCLAVE_CONFIG_FILE", "ENCLAVE_MEM_MB"]
        if self.args.native:
            cmd = [os.path.join(ROOT, "target/release/depot")]
        else:
            envs = ["--env", "ENCLAVE_EGRESS"] if self.args.platform else []
            for k in ["ENCLAVE_CONFIG", "ENCLAVE_PORTS", *secret_envs, "DEPOT_RETIRE_GRACE", "DEPOT_SWEEP_EVERY", "DEPOT_ORPHAN_AGE"]:
                envs += ["--env", k]
            cmd = ["wasmtime", "run", "-S", "inherit-network=y", "-S", "allow-ip-name-lookup=y", "-W",
                   f"max-memory-size={self.args.mem << 20}", *dirs, *envs, self.args.wasm]
        self.log = open(os.path.join(self.work, "depot.log"), "a")
        self.server = subprocess.Popen(cmd, env=env, stdout=self.log, stderr=self.log)
        self.direct = f"http://127.0.0.1:{self.port}"
        self.base = self.direct
        for _ in range(200):
            if self.server.poll() is not None:
                die("depot exited at start: " + open(os.path.join(self.work, "depot.log")).read()[-2000:])
            try:
                urllib.request.urlopen(f"{self.direct}/ping", timeout=1)
                break
            except Exception:
                time.sleep(0.1)
        else:
            die("depot did not come up")
        if self.args.platform:
            self.gw_port = free_port()
            self.gateway = subprocess.Popen(["node", os.path.join(ROOT, "tests/gateway.mjs"), str(self.gw_port),
                                             str(self.port), "keep-auth"], stdout=subprocess.DEVNULL)
            self.procs.append(self.gateway)
            for _ in range(100):
                try:
                    urllib.request.urlopen(f"http://127.0.0.1:{self.gw_port}/ping", timeout=1)
                    break
                except Exception:
                    time.sleep(0.1)
            self.base = f"http://127.0.0.1:{self.gw_port}"

    def stop_depot(self):
        if getattr(self, "gateway", None):
            self.gateway.terminate()
            self.gateway = None
        self.server.terminate()
        self.server.wait(10)
        self.server = None

    def url(self, repo, who=None):
        if who is None:
            return f"{self.base}/{repo}.git"
        return f"http://{who}:{TOKENS[who]}@{self.base.split('://')[1]}/{repo}.git"

    # ---- git -------------------------------------------------------------
    def git(self, *args, cwd=None, ok_codes=(0,), check=True, extra_env=None):
        env = self.git_env if not extra_env else dict(self.git_env, **extra_env)
        r = subprocess.run(["git", *args], cwd=cwd, env=env, capture_output=True, text=True)
        if check and r.returncode not in ok_codes:
            die(f"git {' '.join(args)} -> {r.returncode}\n{r.stdout}\n{r.stderr}\n--- depot log tail ---\n"
                + open(os.path.join(self.work, "depot.log")).read()[-3000:])
        return r

    def api(self, path, who=None, method="GET", body=None):
        req = urllib.request.Request(f"{self.base}{path}", method=method,
                                     data=json.dumps(body).encode() if body is not None else None)
        if who:
            req.add_header("X-Api-Key", TOKENS[who])
        if body is not None:
            req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                return r.status, r.read()
        except urllib.error.HTTPError as e:
            return e.code, e.read()


def refs_of(env, path):
    r = env.git("for-each-ref", "--format=%(objectname) %(refname)", "refs/heads", "refs/tags", cwd=path)
    return sorted(l for l in r.stdout.split("\n") if l)


def remote_refs(env, url, proto="2"):
    r = env.git("-c", f"protocol.version={proto}", "ls-remote", url)
    return sorted(l for l in r.stdout.split("\n") if l and not l.endswith("\tHEAD"))


def make_repo(env, path):
    """A small history with the shapes git servers trip on."""
    env.git("init", "-q", "-b", "main", path)
    w = lambda name, data: open(os.path.join(path, name), "wb").write(data)
    os.makedirs(os.path.join(path, "src/deep/er"), exist_ok=True)
    w("README.md", f"# test {MARK}\n".encode())
    w("src/deep/er/x.txt", b"line\n" * 2000)
    env.git("add", "-A", cwd=path)
    env.git("commit", "-q", "-m", f"first {MARK}", cwd=path)
    for i in range(12):
        with open(os.path.join(path, "src/deep/er/x.txt"), "ab") as f:
            f.write(f"appended {i}\n".encode())
        w(f"file{i}.txt", f"content {i} {MARK}\n".encode() * (i + 1))
        env.git("add", "-A", cwd=path)
        env.git("commit", "-q", "-m", f"commit {i}", cwd=path)
    random.seed(7)
    w("blob.bin", bytes(random.getrandbits(8) for _ in range(300_000)))
    os.symlink("README.md", os.path.join(path, "link"))
    env.git("add", "-A", cwd=path)
    env.git("commit", "-q", "-m", "binary and symlink", cwd=path)
    # a submodule gitlink (the commit lives elsewhere)
    env.git("update-index", "--add", "--cacheinfo", "160000,1234567890123456789012345678901234567890,sub", cwd=path)
    env.git("commit", "-q", "-m", "gitlink", cwd=path)
    env.git("tag", "-a", "v1", "-m", f"release {MARK}", cwd=path)
    env.git("tag", "light", "HEAD~3", cwd=path)
    env.git("checkout", "-q", "-b", "feature", "HEAD~5", cwd=path)
    w("feature.txt", b"feature work\n")
    env.git("add", "-A", cwd=path)
    env.git("commit", "-q", "-m", "feature", cwd=path)
    env.git("checkout", "-q", "main", cwd=path)
    env.git("merge", "-q", "--no-ff", "-m", "merge feature", "feature", cwd=path)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--wasm", default=os.path.join(ROOT, "target/wasm32-wasip2/release/depot.wasm"))
    ap.add_argument("--native", action="store_true")
    ap.add_argument("--keep", action="store_true")
    ap.add_argument("--mirror", help="also push and clone this repository's refs")
    ap.add_argument("--mem", type=int, default=2048, help="guest memory MiB")
    ap.add_argument("--workdir", default=None)
    ap.add_argument("--platform", action="store_true",
                    help="storage through a SOCKS5 egress front, git through a Node gateway like the platform's")
    args = ap.parse_args()
    env = Env(args)
    try:
        run(env, args)
    finally:
        env.close()
    print(f"ALL PASS ({passed} checks)")


def run(env, args):
    print("== setup")
    env.start_minio()
    env.start_hooks()
    env.start_depot()
    ok(f"depot up on {env.base}")
    W = env.work
    src = os.path.join(W, "src")
    make_repo(env, src)

    print("== HTTP: HEAD then GET on one keep-alive connection")
    import http.client
    hc = http.client.HTTPConnection("127.0.0.1", env.port, timeout=10)
    hc.request("HEAD", "/")
    r1 = hc.getresponse()
    r1.read()
    hc.request("GET", "/ping")
    r2 = hc.getresponse()
    if r1.status != 200 or r2.status != 200 or r2.read() != b"ok\n":
        die(f"HEAD/GET pipeline: {r1.status} {r2.status}")
    hc.close()
    ok("HEAD has no body; the connection stays usable")

    print("== auth")
    r = env.git("ls-remote", env.url("w/repo"), check=False)
    if r.returncode == 0:
        die("anonymous ls-remote of a private repo succeeded")
    ok("anonymous read of a private repo refused")
    r = env.git("push", env.url("w/repo", "reader"), "main", cwd=src, check=False)
    if r.returncode == 0 or "403" not in r.stderr:
        die(f"reader push not refused with 403: {r.stderr}")
    ok("reader may not push (403)")
    bad = f"http://writer:wrong-token-0000000000000000@{env.base.split('://')[1]}/w/repo.git"
    r = env.git("ls-remote", bad, check=False)
    if r.returncode == 0:
        die("wrong token accepted")
    ok("wrong token refused")
    r = env.git("push", env.url("other", "writer"), "main", cwd=src, check=False)
    if r.returncode == 0:
        die("writer pushed outside its patterns")
    ok("writer limited to its patterns")

    print("== push (creates the repository)")
    env.git("push", "-q", env.url("w/repo", "writer"), "--all", cwd=src)
    env.git("push", "-q", env.url("w/repo", "writer"), "--tags", cwd=src)
    want = remote_refs(env, env.url("w/repo", "reader"))
    local = sorted(l.replace(" ", "\t") for l in env.git("show-ref", "-d", cwd=src).stdout.split("\n") if l)
    if want != local:
        die(f"advertised refs differ:\n{want}\n{local}")
    ok("pushed refs advertised (v2 ls-refs, peeled tags)")
    if remote_refs(env, env.url("w/repo", "reader"), "0") != want:
        die("v0 advertisement differs from v2")
    ok("v0 advertisement matches")

    print("== clones")
    for proto in ["2", "0"]:
        dst = os.path.join(W, f"clone-v{proto}")
        env.git("-c", f"protocol.version={proto}", "clone", "-q", "--mirror", env.url("w/repo", "reader"), dst)
        env.git("fsck", "--full", "--strict", cwd=dst)
        if refs_of(env, dst) != refs_of(env, src):
            die(f"v{proto} clone refs differ:\n{refs_of(env, dst)}\n{refs_of(env, src)}")
        ok(f"protocol v{proto} mirror clone fscks clean, refs match")
    wc = os.path.join(W, "work")
    env.git("clone", "-q", env.url("w/repo", "writer"), wc)
    if open(os.path.join(wc, "README.md")).read() != f"# test {MARK}\n":
        die("working tree content differs")
    if env.git("rev-parse", "--abbrev-ref", "HEAD", cwd=wc).stdout.strip() != "main":
        die("clone did not check out main (HEAD symref)")
    ok("checkout from clone, HEAD -> main")

    print("== incremental push and fetch (thin packs)")
    with open(os.path.join(src, "src/deep/er/x.txt"), "ab") as f:
        f.write(b"one more line\n")
    env.git("commit", "-qam", "incremental", cwd=src)
    env.git("push", "-q", env.url("w/repo", "writer"), "main", cwd=src)
    env.git("pull", "-q", "--ff-only", cwd=wc)
    if env.git("rev-parse", "HEAD", cwd=wc).stdout != env.git("rev-parse", "HEAD", cwd=src).stdout:
        die("fetch did not bring the new commit")
    env.git("fsck", "--full", cwd=wc)
    ok("thin push + incremental fetch")
    for proto in ["0", "2"]:
        with open(os.path.join(wc, f"n{proto}.txt"), "w") as f:
            f.write("x\n")
        env.git("add", "-A", cwd=wc)
        env.git("commit", "-qm", f"from clone v{proto}", cwd=wc)
        env.git("-c", f"protocol.version={proto}", "push", "-q", "origin", "main", cwd=wc)
        env.git("-c", f"protocol.version={proto}", "fetch", "-q", env.url("w/repo", "reader"), "main", cwd=src)
        if env.git("rev-parse", "FETCH_HEAD", cwd=src).stdout != env.git("rev-parse", "HEAD", cwd=wc).stdout:
            die(f"v{proto} fetch into the original missed the commit")
    env.git("merge", "-q", "--ff-only", "FETCH_HEAD", cwd=src)
    env.git("fsck", "--full", cwd=src)
    ok("negotiated fetches (v0 multi_ack_detailed, v2) into an existing repo")

    print("== webhooks")
    import hmac as _hmac
    for _ in range(50):
        if env.deliveries:
            break
        time.sleep(0.1)
    if not env.deliveries:
        die("no webhook delivery after pushes")
    hdrs, body = env.deliveries[-1]
    want = "sha256=" + _hmac.new(HOOK_SECRET.encode(), body, hashlib.sha256).hexdigest()
    if hdrs.get("x-depot-signature") != want or hdrs.get("x-hub-signature-256") != want:
        die("webhook signature does not verify")
    ev = json.loads(body)
    if ev["repository"] != "w/repo" or not ev["updates"] or ev["event"] != "push":
        die(f"webhook body: {ev}")
    if any(json.loads(b)["repository"].startswith("pub/") for _, b in env.deliveries):
        die("a hook fired outside its repo patterns")
    ok(f"{len(env.deliveries)} signed push webhooks (GitHub-compatible signature)")

    print("== shallow")
    for proto in ["2", "0"]:
        sh = os.path.join(W, f"shallow-v{proto}")
        g = ["-c", f"protocol.version={proto}"]
        env.git(*g, "clone", "-q", "--depth", "1", env.url("w/repo", "reader"), sh)
        n = int(env.git("rev-list", "--count", "HEAD", cwd=sh).stdout)
        if n != 1:
            die(f"depth 1 clone has {n} commits")
        env.git(*g, "fetch", "-q", "--deepen", "3", cwd=sh)
        n2 = int(env.git("rev-list", "--count", "HEAD", cwd=sh).stdout)
        if n2 <= 1:
            die("deepen did not deepen")
        env.git(*g, "fetch", "-q", "--unshallow", cwd=sh)
        full = int(env.git("rev-list", "--count", "HEAD", cwd=src).stdout)
        if int(env.git("rev-list", "--count", "HEAD", cwd=sh).stdout) != full:
            die("unshallow is not complete")
        env.git("fsck", "--full", cwd=sh)
        ok(f"v{proto}: depth 1 -> deepen 3 ({n2}) -> unshallow ({full}), fsck clean")
    since = os.path.join(W, "since")
    env.git("clone", "-q", "--shallow-since=2000-01-01", env.url("w/repo", "reader"), since)
    ok("--shallow-since clone")
    excl = os.path.join(W, "exclude")
    env.git("clone", "-q", "--shallow-exclude=light", env.url("w/repo", "reader"), excl)
    light = env.git("rev-parse", "light", cwd=src).stdout.strip()
    r = env.git("cat-file", "-e", light, cwd=excl, check=False)
    if r.returncode == 0:
        die("--shallow-exclude still sent the excluded commit")
    env.git("fsck", "--full", cwd=excl)
    ok("--shallow-exclude (deepen-not) clone")

    print("== protection, atomic, deletes")
    env.git("checkout", "-q", "-b", "scratch", cwd=src)
    env.git("commit", "-q", "--allow-empty", "-m", "scratch", cwd=src)
    env.git("push", "-q", env.url("w/repo", "writer"), "scratch", cwd=src)
    env.git("reset", "-q", "--hard", "HEAD~2", cwd=src)
    env.git("push", "-q", "-f", env.url("w/repo", "writer"), "scratch", cwd=src)
    ok("force push to an unprotected branch")
    r = env.git("push", "-f", env.url("w/repo", "writer"), "scratch:main", cwd=src, check=False)
    if r.returncode == 0 or "protected" not in r.stderr:
        die(f"force push to protected main was not refused: {r.stderr}")
    ok("force push to protected main refused")
    r = env.git("push", env.url("w/repo", "writer"), ":main", cwd=src, check=False)
    if r.returncode == 0:
        die("deleting protected main succeeded")
    ok("deleting protected main refused")
    r = env.git("push", "-f", env.url("w/repo", "writer"), "HEAD:refs/tags/v1", cwd=src, check=False)
    if r.returncode == 0:
        die("moving a protected tag succeeded")
    ok("protected tags are immutable")
    r = env.git("push", "--atomic", "-f", env.url("w/repo", "writer"), "HEAD:refs/heads/atom", "scratch:main",
                cwd=src, check=False)
    if r.returncode == 0:
        die("atomic push with a refused ref succeeded")
    if "refs/heads/atom" in "".join(remote_refs(env, env.url("w/repo", "reader"))):
        die("atomic push applied part of itself")
    ok("atomic push is all-or-nothing")
    env.git("push", "-q", env.url("w/repo", "writer"), ":scratch", cwd=src)
    if "refs/heads/scratch" in "".join(remote_refs(env, env.url("w/repo", "reader"))):
        die("branch delete did not take")
    ok("delete-only push")
    env.git("checkout", "-q", "main", cwd=src)

    print("== fetch by object id")
    sha = env.git("rev-parse", "HEAD~4", cwd=src).stdout.strip()
    fresh = os.path.join(W, "byid")
    env.git("init", "-q", fresh)
    env.git("fetch", "-q", env.url("w/repo", "reader"), sha, cwd=fresh)
    if env.git("cat-file", "-t", sha, cwd=fresh).stdout.strip() != "commit":
        die("fetch by sha failed")
    ok("fetch of a reachable non-tip commit")

    print("== large objects (multipart, streamed windows)")
    big = os.path.join(W, "big")
    env.git("init", "-q", "-b", "main", big)
    with open(os.path.join(big, "big.bin"), "wb") as f:
        f.write(os.urandom(24 << 20))
    env.git("add", "-A", cwd=big)
    env.git("commit", "-qm", "24 MiB of noise", cwd=big)
    env.git("push", "-q", env.url("w/big", "writer"), "main", cwd=big)
    bigc = os.path.join(W, "big-clone")
    env.git("clone", "-q", env.url("w/big", "reader"), bigc)
    a = hashlib.sha256(open(os.path.join(big, "big.bin"), "rb").read()).hexdigest()
    b = hashlib.sha256(open(os.path.join(bigc, "big.bin"), "rb").read()).hexdigest()
    if a != b:
        die("large blob differs after round trip")
    ok("24 MiB blob round-trips")

    print("== public and empty repositories")
    env.git("push", "-q", env.url("pub/open", "admin"), "main", cwd=src)
    env.git("clone", "-q", env.url("pub/open"), os.path.join(W, "anon"))
    ok("anonymous clone of a public repo")
    st, _ = env.api("/api/repos", "admin", "POST", {"name": "empty"})
    if st != 201:
        die(f"create returned {st}")
    r = env.git("clone", env.url("empty", "reader"), os.path.join(W, "empty"), check=False)
    if r.returncode != 0 or "empty repository" not in r.stderr:
        die(f"empty clone: {r.stderr}")
    ok("empty repository clones with git's warning")

    if shutil.which("cargo"):
        print("== libgit2 (cargo git dependencies): clone, then an incremental update")
        crate = os.path.join(W, "mini")
        env.git("init", "-q", "-b", "main", crate)
        os.makedirs(os.path.join(crate, "src"))
        open(os.path.join(crate, "Cargo.toml"), "w").write('[package]\nname = "mini"\nversion = "0.1.0"\nedition = "2021"\n')
        open(os.path.join(crate, "src/lib.rs"), "w").write("pub fn one() -> u32 { 1 }\n")
        env.git("add", "-A", cwd=crate)
        env.git("commit", "-qm", "mini", cwd=crate)
        env.git("push", "-q", env.url("pub/mini", "admin"), "main", cwd=crate)
        cons = os.path.join(W, "consumer")
        os.makedirs(os.path.join(cons, "src"))
        open(os.path.join(cons, "src/main.rs"), "w").write("fn main() { println!(\"{}\", mini::one()); }\n")
        open(os.path.join(cons, "Cargo.toml"), "w").write(
            f'[package]\nname = "consumer"\nversion = "0.1.0"\nedition = "2021"\n[dependencies]\nmini = {{ git = "{env.url("pub/mini")}" }}\n')
        cenv = dict(os.environ, CARGO_NET_GIT_FETCH_WITH_CLI="false", CARGO_HOME=os.path.join(W, "cargo-home"))
        for step in ("fetch", "update"):
            if step == "update":
                with open(os.path.join(crate, "src/lib.rs"), "a") as f:
                    f.write("pub fn two() -> u32 { 2 }\n")
                env.git("commit", "-qam", "two", cwd=crate)
                env.git("push", "-q", env.url("pub/mini", "admin"), "main", cwd=crate)
            r = subprocess.run(["cargo", step], cwd=cons, env=cenv, capture_output=True, text=True, timeout=300)
            if r.returncode:
                die(f"cargo {step} (libgit2) failed: {r.stderr[-1500:]}")
        head = env.git("rev-parse", "HEAD", cwd=crate).stdout.strip()
        if head not in open(os.path.join(cons, "Cargo.lock")).read():
            die("cargo update did not pick up the new commit")
        ok("cargo's libgit2 clones and incrementally updates a git dependency (protocol v0)")

    print("== web API")
    st, body = env.api("/api/repos", "reader")
    names = [x["name"] for x in json.loads(body)["repos"]]
    if not {"w/repo", "w/big", "pub/open", "empty"} <= set(names):
        die(f"repo list: {names}")
    st, body = env.api("/api/repos")
    anon = [x["name"] for x in json.loads(body)["repos"]]
    if "pub/open" not in anon or any(not n.startswith("pub/") for n in anon):
        die(f"anonymous repo list: {body}")
    ok("repo listing filtered by access")
    st, body = env.api("/api/log?repo=w/repo&n=5", "reader")
    log = json.loads(body)["commits"]
    if len(log) != 5 or log[0]["id"] != env.git("rev-parse", "HEAD", cwd=src).stdout.strip():
        die(f"log: {body[:400]}")
    st, body = env.api("/api/tree?repo=w/repo&path=src/deep", "reader")
    if json.loads(body)["entries"][0]["name"] != "er":
        die(f"tree: {body[:300]}")
    st, body = env.api("/api/raw?repo=w/repo&path=README.md", "reader")
    if body.decode() != f"# test {MARK}\n":
        die(f"raw: {body[:200]}")
    st, body = env.api("/api/commit?repo=w/repo&id=" + log[0]["id"], "reader")
    if "changes" not in json.loads(body):
        die("commit view")
    ok("log, tree, raw and commit views")

    print("== minted tokens over X-Api-Key (how git authenticates behind the platform gateway)")
    st, body = env.api("/api/tokens", "admin", "POST", {"user": "ci", "read": ["w/*"], "note": "e2e ci"})
    if st != 201:
        die(f"mint: {st} {body}")
    minted = json.loads(body)
    tok, tid = minted["token"], minted["id"]
    hdr = ["-c", f"http.extraHeader=X-Api-Key: {tok}"]
    viahdr = os.path.join(W, "via-header")
    env.git(*hdr, "clone", "-q", env.url("w/repo"), viahdr)
    ok("clone with a minted token in X-Api-Key (http.extraHeader)")
    r = env.git(*hdr, "push", env.url("w/repo"), "main:refs/heads/ci-try", cwd=viahdr, check=False)
    if r.returncode == 0 or "403" not in r.stderr:
        die(f"read-only token pushed: {r.stderr}")
    st, body = env.api("/api/tokens", "admin")
    if not any(t["id"] == tid and "hash" not in t for t in json.loads(body)["tokens"]):
        die("token listing")
    st, _ = env.api("/api/tokens", "reader")
    if st != 403:
        die("non-admin listed tokens")
    st, _ = env.api(f"/api/tokens?id={tid}", "admin", "DELETE")
    if st != 200:
        die("revoke")
    time.sleep(0.1)
    r = env.git(*hdr, "ls-remote", env.url("w/repo"), check=False)
    if r.returncode == 0:
        die("revoked token still works")
    ok("read-only scope enforced, listing hides hashes, revocation immediate")

    print("== concurrency: clones and pushes at once")
    import threading
    results = {}

    def job(key, *gitargs, cwd=None):
        results[key] = env.git(*gitargs, cwd=cwd, check=False)

    racers = []
    for i in range(2):
        d = os.path.join(W, f"racer{i}")
        env.git("clone", "-q", env.url("w/repo", "writer"), d)
        with open(os.path.join(d, f"race{i}.txt"), "w") as f:
            f.write(os.urandom(200_000).hex())
        env.git("add", "-A", cwd=d)
        env.git("commit", "-qm", f"race {i}", cwd=d)
        racers.append(d)
    threads = [threading.Thread(target=job, args=(f"clone{i}", "clone", "-q", "--mirror", env.url("w/repo", "reader"),
                                                   os.path.join(W, f"cc{i}"))) for i in range(4)]
    threads += [threading.Thread(target=job, args=(f"branch{i}", "push", "-q", "origin", f"HEAD:refs/heads/race{i}"),
                                 kwargs={"cwd": d}) for i, d in enumerate(racers)]
    threads += [threading.Thread(target=job, args=(f"main{i}", "push", "-q", "origin", "HEAD:refs/heads/contend"),
                                 kwargs={"cwd": d}) for i, d in enumerate(racers)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    for k, r in results.items():
        if k.startswith(("clone", "branch")) and r.returncode != 0:
            die(f"concurrent {k} failed: {r.stderr}")
    contend = [results["main0"].returncode, results["main1"].returncode]
    if 0 not in contend:
        die(f"both contending pushes failed: {results['main0'].stderr} {results['main1'].stderr}")
    for i in range(4):
        env.git("fsck", "--full", cwd=os.path.join(W, f"cc{i}"))
    refs = "".join(remote_refs(env, env.url("w/repo", "reader")))
    if "refs/heads/race0" not in refs or "refs/heads/race1" not in refs:
        die("a concurrent branch push was lost")
    ok(f"4 clones + 4 pushes concurrently: clones fsck clean, both branches land, contended ref {contend}")

    if shutil.which("cast"):
        print("== Sign in with Enclave (EST1 tokens)")
        now = int(time.time())
        good = mint_est1(SSO_SUB, SSO_AUD, now, now + 3600)
        hdr = lambda t: ["-c", f"http.extraHeader=X-Sso-Token: {t}"]
        req = urllib.request.Request(f"{env.base}/api/whoami", headers={"X-Sso-Token": good})
        me = json.loads(urllib.request.urlopen(req, timeout=10).read())
        if me["user"] != "signed" or not me["signed_in"]:
            die(f"sign-in did not map to the configured user: {me}")
        env.git(*hdr(good), "ls-remote", env.url("w/repo"))
        r = env.git(*hdr(good), "push", env.url("w/repo"), "main:refs/heads/sso-try", cwd=src, check=False)
        if r.returncode == 0:
            die("a read-only signed-in user pushed")
        for label, bad in [("another deployment", mint_est1(SSO_SUB, "0x" + "22" * 32, now, now + 3600)),
                           ("expired", mint_est1(SSO_SUB, SSO_AUD, now - 7200, now - 3600)),
                           ("wrong signer", mint_est1(SSO_SUB, SSO_AUD, now, now + 3600, "0x" + "43" * 32))]:
            if env.git(*hdr(bad), "ls-remote", env.url("w/repo"), check=False).returncode == 0:
                die(f"a sign-in token for {label} was accepted")
        ok("EST1 sign-in maps to its configured user; wrong audience, expired and forged tokens refused")

    print("== repack: many pushes merge into few packs")
    many = os.path.join(W, "many")
    env.git("init", "-q", "-b", "main", many)
    for i in range(9):
        with open(os.path.join(many, f"f{i}.txt"), "w") as f:
            f.write(f"{i}\n" * (1000 * (i + 1)))
        env.git("add", "-A", cwd=many)
        env.git("commit", "-qm", f"c{i}", cwd=many)
        env.git("push", "-q", env.url("w/many", "writer"), "main", cwd=many)
    for _ in range(100):
        st, body = env.api("/api/repo?repo=w/many", "admin")
        if json.loads(body)["packs"] < 9:
            break
        time.sleep(0.2)
    packs = json.loads(body)["packs"]
    if packs >= 9:
        die("no repack happened")
    st, body = env.api("/api/status", "admin")
    ok(f"9 pushes -> {packs} packs ({json.loads(body)['maintenance']['last']})")
    mc = os.path.join(W, "many-clone")
    env.git("clone", "-q", env.url("w/many", "reader"), mc)
    env.git("fsck", "--full", cwd=mc)
    if env.git("rev-parse", "HEAD", cwd=mc).stdout != env.git("rev-parse", "HEAD", cwd=many).stdout:
        die("clone after repack differs")
    with open(os.path.join(many, "after.txt"), "w") as f:
        f.write("after repack\n")
    env.git("add", "-A", cwd=many)
    env.git("commit", "-qm", "after repack", cwd=many)
    env.git("push", "-q", env.url("w/many", "writer"), "main", cwd=many)
    env.git("pull", "-q", cwd=mc)
    env.git("fsck", "--full", cwd=mc)
    ok("clone, thin push and fetch across the merged pack")
    # retired packs disappear after the (test) grace
    time.sleep(3)
    names = [x["name"] for x in json.loads(env.api("/api/repos", "admin")[1])["repos"]]
    total_packs = sum(json.loads(env.api(f"/api/repo?repo={n}", "admin")[1])["packs"] for n in names)
    for _ in range(50):
        stored = len([k for k in env.bucket_objects() if k.endswith(".pack")])
        if stored == total_packs:
            break
        time.sleep(0.2)
    if stored != total_packs:
        die(f"bucket holds {stored} packs, manifests list {total_packs}: retired packs were not swept")
    ok(f"retired packs swept: bucket holds exactly the {stored} listed packs")
    # an upload that never committed (a push killed mid-way) is swept after its age
    rid = next(k for k in env.bucket_objects() if k.endswith("/manifest")).split("/")[2]
    junk = os.path.join(W, "junk.bin")
    open(junk, "wb").write(os.urandom(1000))
    orphan = f"/{BUCKET}/depot/r/{rid}/0123456789abcdef0123456789abcdef.pack"
    subprocess.run(["curl", "-s", "-o", "/dev/null", "-X", "PUT", "--aws-sigv4", "aws:amz:us-east-1:s3", "--user",
                    f"{AK}:{SK}", "-T", junk, env.s3 + orphan], check=True)
    for _ in range(150):
        if not any(k.endswith("0123456789abcdef0123456789abcdef.pack") for k in env.bucket_objects()):
            break
        time.sleep(0.1)
    else:
        die("an orphaned pack object was never swept")
    if len([k for k in env.bucket_objects() if k.endswith(".pack")]) != total_packs:
        die("the orphan sweep removed a listed pack")
    ok("orphaned uploads are swept; listed packs are untouched")

    print("== restart: everything comes back from the bucket")
    env.stop_depot()
    env.start_depot()
    again = os.path.join(W, "after-restart")
    env.git("clone", "-q", "--mirror", env.url("w/repo", "reader"), again)
    env.git("fsck", "--full", cwd=again)
    if remote_refs(env, env.url("w/repo", "reader")) != sorted(
            l.replace(" ", "\t") for l in env.git("show-ref", "-d", cwd=again).stdout.split("\n") if l):
        die("refs after restart differ")
    ok("clone after restart, fsck clean")
    st, _ = env.api("/api/status", "admin")
    ok(f"status {st}")

    print("== the bucket holds no plaintext")
    keys = env.bucket_objects()
    if not keys or any("repo" in k or "main" in k or "big" in k for k in keys):
        die(f"object keys leak names: {keys[:10]}")
    needles = [MARK.encode(), b"refs/heads", b"w/repo", b"PACK\x00\x00\x00\x02", b"tree "]

    def scan(top):
        hits = []
        for dirpath, _, files in os.walk(top):
            for fn in files:
                data = open(os.path.join(dirpath, fn), "rb").read()
                hits += [(n, fn) for n in needles if n in data]
        return hits

    # control: the same scan over a bucket holding a plaintext copy must find it
    env.s3curl("PUT", "/control")
    ctl = os.path.join(W, "control.bin")
    open(ctl, "wb").write(b"PACK\x00\x00\x00\x02" + f"tree refs/heads w/repo {MARK}".encode() * 50)
    subprocess.run(["curl", "-s", "-o", "/dev/null", "-X", "PUT", "--aws-sigv4", "aws:amz:us-east-1:s3", "--user",
                    f"{AK}:{SK}", "-T", ctl, f"{env.s3}/control/plain"], check=True)
    if len({n for n, _ in scan(os.path.join(env.minio_dir, "control"))}) != len(needles):
        die("the plaintext scanner misses a plaintext object; this check would prove nothing")
    hits = scan(os.path.join(env.minio_dir, BUCKET))
    if hits:
        die(f"plaintext found in the bucket: {hits[:5]}")
    ok(f"{len(keys)} objects, none readable: no names, refs, packs or content")

    print("== export: the bucket back to plain git without depot")
    out = os.path.join(W, "export")
    r = subprocess.run([sys.executable, os.path.join(ROOT, "scripts/depot-export.py"), "--endpoint", env.s3, "--bucket", BUCKET,
                        "--prefix", "depot/", "--region", "us-east-1", "--out", out],
                       env=dict(os.environ, DEPOT_MASTER_KEY="m" * 40, S3_ACCESS_KEY=AK, S3_SECRET_KEY=SK),
                       capture_output=True, text=True)
    if r.returncode:
        die(f"export failed: {r.stderr}{r.stdout}")
    for name in ["w/repo", "w/many", "pub/open", "w/big"]:
        got = sorted(l.replace(" ", "\t") for l in env.git("show-ref", "-d", cwd=os.path.join(out, name + ".git")).stdout.split("\n") if l)
        if got != remote_refs(env, env.url(name, "reader")):
            die(f"exported {name} refs differ")
    ok(f"export decrypts every repository into bare git, fsck clean, refs identical ({r.stdout.count('fsck clean')} repos)")

    print("== wrong master key cannot read the bucket")
    env.stop_depot()
    os.environ["E2E_MASTER_OVERRIDE"] = "1"
    port = free_port()
    e = dict(os.environ, ENCLAVE_CONFIG=env.config(), ENCLAVE_PORTS=f"http:8000={port}", E2E_AK=AK, E2E_SK=SK,
             E2E_MASTER="x" * 40, E2E_TOK_ADMIN=TOKENS["admin"], E2E_TOK_WRITER=TOKENS["writer"], E2E_HOOK=HOOK_SECRET)
    if args.native:
        cmd = [os.path.join(ROOT, "target/release/depot")]
    else:
        cmd = ["wasmtime", "run", "-S", "inherit-network=y", "-S", "allow-ip-name-lookup=y"]
        for k in ["ENCLAVE_CONFIG", "ENCLAVE_PORTS", "E2E_AK", "E2E_SK", "E2E_MASTER", "E2E_TOK_ADMIN", "E2E_TOK_WRITER", "E2E_HOOK"]:
            cmd += ["--env", k]
        cmd.append(args.wasm)
    p = subprocess.run(cmd, env=e, capture_output=True, text=True, timeout=60)
    if p.returncode == 0 or "registry" not in p.stderr:
        die(f"wrong key did not stop startup: {p.stderr[-500:]}")
    ok("a server with the wrong master key refuses to start")
    env.start_depot()

    if args.platform:
        print("== platform: a gateway that strips Authorization, storage via the egress front")
        gw = free_port()
        p = subprocess.Popen(["node", os.path.join(ROOT, "tests/gateway.mjs"), str(gw), str(env.port)], stdout=subprocess.DEVNULL)
        env.procs.append(p)
        time.sleep(0.5)
        r = env.git("ls-remote", f"http://reader:{TOKENS['reader']}@127.0.0.1:{gw}/w/repo.git", check=False)
        if r.returncode == 0:
            die("Basic credentials survived a stripping gateway?")
        r = env.git("-c", f"http.extraHeader=X-Api-Key: {TOKENS['reader']}", "clone", "-q",
                    f"http://127.0.0.1:{gw}/w/repo.git", os.path.join(W, "stripped"))
        ok("behind a stripping gateway: Basic fails, X-Api-Key clones")
        relayed = int(open(env.socks_count).read())
        if relayed < 2:
            die("storage traffic did not go through the egress front")
        ok(f"storage reached through the SOCKS5 egress front ({relayed} connections)")

    if args.mirror:
        mirror(env, args.mirror)


def mirror(env, path):
    print(f"== mirror {path}")
    W = env.work
    bare = os.path.join(W, "mirror-src.git")
    env.git("clone", "-q", "--bare", "--no-local", path, bare)
    t0 = time.time()
    r = env.git("push", "--mirror", env.url("w/mirror", "writer"), cwd=bare)
    ok(f"mirror push {time.time() - t0:.1f}s")
    t0 = time.time()
    dst = os.path.join(W, "mirror-clone.git")
    env.git("clone", "-q", "--mirror", env.url("w/mirror", "reader"), dst)
    ok(f"mirror clone {time.time() - t0:.1f}s")
    env.git("fsck", "--full", cwd=dst)
    a = env.git("for-each-ref", cwd=bare).stdout
    b = env.git("for-each-ref", cwd=dst).stdout
    if a != b:
        die("mirror refs differ")
    ok("mirror refs identical, fsck clean")


if __name__ == "__main__":
    main()
