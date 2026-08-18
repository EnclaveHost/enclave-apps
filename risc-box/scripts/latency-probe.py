#!/usr/bin/env python3
"""Input-to-pixel latency probe for a RISC Box deployment.

Measures keypress -> first dirty band with the PRODUCTION choreography: the
/fb.bands?wait=1 long-poll is parked BEFORE the input goes out, exactly as
the bridge and web console hold one. Two input channels, A/B-able:
  hid     one warm pipelined POST /hid per sample
  stream  one long-lived chunked POST /hid-stream, one line per sample
Requires a QUIET screen with a focused terminal (the keystroke must be the
only thing painting). Usage: latency-probe.py HOST TOKENFILE MODE N
"""
import http.client, json, socket, ssl, statistics, sys, time

HOST = sys.argv[1]
TOKEN = open(sys.argv[2]).read().strip()
MODE = sys.argv[3]
N = int(sys.argv[4]) if len(sys.argv) > 4 else 30

H = {"Authorization": "Bearer " + TOKEN, "x-api-key": TOKEN}
ctx = ssl.create_default_context()

pull = http.client.HTTPSConnection(HOST, 443, context=ctx, timeout=15)

def pull_send(since, wait=1):
    pull.request("GET", f"/fb.bands?since={since}&wait={wait}", headers=H)

def pull_read():
    r = pull.getresponse()
    j = json.loads(r.read())
    return j["gen"], len(j.get("events", []))

def do_pull(since, wait=1):
    pull_send(since, wait)
    return pull_read()

if MODE == "hid":
    post = http.client.HTTPSConnection(HOST, 443, context=ctx, timeout=15)
    def do_move(x, y):
        body = json.dumps({"events": [{"t": "key", "code": 30, "down": True}, {"t": "key", "code": 30, "down": False}]})
        t0 = time.time()
        post.request("POST", "/hid", body=body, headers={**H, "content-type": "application/json"})
        post.getresponse().read()
        return t0
elif MODE == "stream":
    raw = socket.create_connection((HOST, 443), timeout=15)
    s = ctx.wrap_socket(raw, server_hostname=HOST)
    head = (f"POST /hid-stream HTTP/1.1\r\nHost: {HOST}\r\n"
            f"Authorization: Bearer {TOKEN}\r\nx-api-key: {TOKEN}\r\n"
            f"Transfer-Encoding: chunked\r\ncontent-type: text/plain\r\n\r\n")
    s.sendall(head.encode())
    s.settimeout(0.05)
    def do_move(x, y):
        line = json.dumps({"events": [{"t": "key", "code": 30, "down": True}, {"t": "key", "code": 30, "down": False}]}) + "\n"
        b = line.encode()
        frame = f"{len(b):x}\r\n".encode() + b + b"\r\n"
        t0 = time.time()
        s.sendall(frame)
        return t0
else:
    sys.exit("mode must be hid|stream")

# settle: drain the ring, wait for 1s of stillness
gen, _ = do_pull(0, wait=0)
quiet = time.time()
while time.time() - quiet < 1.0:
    g2, n = do_pull(gen, wait=0)
    if n:
        gen, quiet = g2, time.time()
    time.sleep(0.02)

# early-death check for the stream: a 501/4xx answer arrives within 50ms
if MODE == "stream":
    try:
        peek = s.recv(256)
        print("stream answered early (fix not live?):", peek[:80], file=sys.stderr)
        sys.exit(2)
    except (socket.timeout, TimeoutError):
        pass  # silence = parked = good

samples = []
pos = [(0.25, 0.25), (0.75, 0.75)]
for i in range(N):
    pull_send(gen)          # park the long-poll first, like the bridge
    time.sleep(0.02)        # let the park land server-side
    t0 = do_move(*pos[i % 2])
    deadline = t0 + 3.0
    got = None
    pending = True
    while time.time() < deadline:
        try:
            g2, n = pull_read()
        except Exception:
            pending = False
            break
        pending = False
        if n:
            got = time.time()
            gen = g2
            break
        pull_send(gen)
        pending = True
    if pending:
        # a parked request is still outstanding: the connection cannot take
        # another send, so start over on a fresh one
        pull.close()
        pull = http.client.HTTPSConnection(HOST, 443, context=ctx, timeout=15)
    if got:
        samples.append((got - t0) * 1000)
    # let the screen still again between samples
    while True:
        g2, n = do_pull(gen, wait=0)
        if not n:
            break
        gen = g2
    time.sleep(0.08)

samples.sort()
if samples:
    print(f"{MODE}: n={len(samples)} median={statistics.median(samples):.1f}ms "
          f"p25={samples[len(samples)//4]:.1f} p75={samples[3*len(samples)//4]:.1f} "
          f"min={samples[0]:.1f} max={samples[-1]:.1f}")
else:
    print(f"{MODE}: no samples (screen never moved)")
