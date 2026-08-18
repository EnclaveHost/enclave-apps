#!/usr/bin/env python3
"""Input-to-pixel latency probe for a RISC Box deployment.

Requires a QUIET screen (no demo running): the cursor move must be the only
thing changing. One warm connection holds a /fb.bands pull loop; another
sends an absolute mouse move; the sample is t(first non-empty pull response
after the move) - t(move POST written).
"""
import http.client, json, ssl, sys, time

HOST = sys.argv[1] if len(sys.argv) > 1 else "458a63b9.app.enclave.host"
TOKEN = open(sys.argv[2]).read().strip() if len(sys.argv) > 2 else None
N = int(sys.argv[3]) if len(sys.argv) > 3 else 20
POLL_SLEEP = float(sys.argv[4]) if len(sys.argv) > 4 else 0.004

H = {}
if TOKEN:
    H["Authorization"] = "Bearer " + TOKEN
    H["x-api-key"] = TOKEN

ctx = ssl.create_default_context()

def conn():
    c = http.client.HTTPSConnection(HOST, 443, context=ctx, timeout=15)
    return c

pull = conn()
post = conn()

def do_pull(since):
    pull.request("GET", f"/fb.bands?since={since}", headers=H)
    r = pull.getresponse()
    body = r.read()
    j = json.loads(body)
    return j["gen"], len(j.get("events", [])), sum(len(e.get("b", "")) for e in j.get("events", []))

def do_move(x, y):
    body = json.dumps({"events": [{"t": "move", "x": x, "y": y}]})
    t0 = time.time()
    post.request("POST", "/hid", body=body, headers={**H, "content-type": "application/json"})
    r = post.getresponse()
    r.read()
    return t0

# settle: drain the ring and wait for stillness (no events for 1s)
gen, _, _ = do_pull(0)
quiet = time.time()
while time.time() - quiet < 1.0:
    g2, n, _ = do_pull(gen)
    if n:
        gen = g2
        quiet = time.time()
    time.sleep(0.02)

samples = []
pos = [(0.25, 0.25), (0.75, 0.75)]
for i in range(N):
    t0 = do_move(*pos[i % 2])
    deadline = t0 + 3.0
    got = None
    while time.time() < deadline:
        g2, n, b = do_pull(gen)
        if n:
            got = time.time()
            gen = g2
            break
        gen = g2
        time.sleep(POLL_SLEEP)
    if got:
        samples.append((got - t0) * 1000)
    else:
        print(f"sample {i}: TIMEOUT (screen never changed)")
    # let the screen go quiet again
    time.sleep(0.25)
    gen, _, _ = do_pull(gen)

if samples:
    s = sorted(samples)
    n = len(s)
    print(f"input->pixel-at-client over {n} samples (poll sleep {POLL_SLEEP*1000:.0f}ms):")
    print(f"  min {s[0]:.0f}  p25 {s[n//4]:.0f}  median {s[n//2]:.0f}  p75 {s[3*n//4]:.0f}  max {s[-1]:.0f}  (ms)")
