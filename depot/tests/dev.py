#!/usr/bin/env python3
"""A local depot to look at: MinIO + the wasm build, with sample repositories
pushed (this app's own history and anything passed with --repo). Prints the
URL and the admin token, then serves until interrupted.
  python3 tests/dev.py [--repo PATH ...] [--port N]"""
import argparse
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import e2e  # noqa: E402

ap = argparse.ArgumentParser()
ap.add_argument("--repo", action="append", default=[])
ap.add_argument("--wasm", default=os.path.join(e2e.ROOT, "target/wasm32-wasip2/release/depot.wasm"))
ap.add_argument("--workdir", default=None)
ap.add_argument("--port", type=int, default=0)
a = ap.parse_args()
a.native = a.keep = a.platform = False
a.mirror = None
a.mem = 2048
env = e2e.Env(a)
try:
    env.start_minio()
    env.start_hooks()
    if a.port:
        e2e.free_port = lambda: a.port
    env.start_depot()
    for path in [os.path.dirname(e2e.ROOT)] + a.repo:
        name = "demo/" + os.path.basename(os.path.abspath(path))
        bare = os.path.join(env.work, os.path.basename(path) + ".git")
        env.git("clone", "-q", "--bare", "--no-local", path, bare)
        env.git("push", "-q", "--mirror", env.url(name, "admin"), cwd=bare)
        print("pushed", name)
    env.api("/api/repo?repo=" + "demo/" + os.path.basename(os.path.dirname(e2e.ROOT)), "admin", "PATCH",
            {"public": True, "description": "Example apps for enclave.host"})
    print(f"\n{env.base}\nadmin token: {e2e.TOKENS['admin']}\n", flush=True)
    while True:
        time.sleep(3600)
except KeyboardInterrupt:
    pass
finally:
    env.close()
