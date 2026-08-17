# Handoff: one request silences an open SSE stream, for good

> **RESOLVED 2026-08-16 — it was BOTH layers, stacked, and the trigger was a
> red herring.** Bisection per this doc's protocol: the app, run locally under
> plain wasmtime (idle AND with a running guest), never drops a heartbeat
> through any number of `/status` triggers — and a lone stream against the
> deployment with the box otherwise silent died at t+2.9s with no second
> request at all, so "any other request wedges it" was coincidence of timing.
> The mechanism, both halves measured:
>
> 1. **App (`src/httpd.rs`)**: over a real backpressured path the SSE write
>    buffer rides near the 192 KiB pacing gate, and one large band burst (a
>    whole-screen repaint) jumps it straight past `MAX_WBUF` (512 KiB), where
>    `flush()` CLOSED the connection. On loopback the buffer never fills,
>    which is why the app looked innocent locally and why the local test in
>    this doc, run alone, gives the wrong verdict.
> 2. **Platform (`~/Projects/enclave/supervisor.js`, the `/x/:id` proxy)**:
>    `upRes.pipe(res)` forwards only a CLEAN upstream end. The app's close
>    arrived as an aborted upstream response — unhandled — so the pipe simply
>    stopped and the CLIENT socket stayed ESTABLISHED and silent forever: no
>    FIN, no error, no heartbeat. Reproduced locally with the handler's exact
>    code shape: app killed mid-stream, client hung 25s+ until its own
>    timeout. That swallow is what turned a recoverable stream-end (an
>    EventSource redials instantly on a visible close) into the permanent
>    wedge this doc describes.
>
> Fixes, verified locally: httpd.rs now STARVES an over-backlog SSE
> subscriber instead of closing it (skip past `SSE_SKIP_WBUF`, resume below
> `SSE_RESUME_WBUF`, `sse_take_recovered()` owes it a full frame/keyframe;
> the stall reaper now keys on "nothing drained for 45s", not "non-empty for
> 45s"), and the supervisor/api-relay proxies destroy the client leg when the
> tenant leg dies mid-response (abort propagates in ~20 ms, measured). The
> cold-connect collapse was re-measured absent with the box quiet and is
> state-dependent on wedge activity; re-check it after both fixes deploy.
> Confirmation on a live deployment still requires shipping both layers.

You are picking up a reproducible defect on RISC Box (`~/Projects/enclave-apps/risc-box`;
platform repo `~/Projects/enclave`). Everything below was measured, not assumed.
Read the ruled-out list before repeating any of it.

## The defect

**An open SSE stream stops delivering, permanently, as soon as any other HTTP
request touches the same deployment.** The socket stays `ESTABLISHED` with an
empty receive queue. No FIN, no RST, no error, no heartbeat. The client has no
way to notice except a timeout.

Reproduce with two curls and no app code — this is the whole bug:

```bash
B=https://e64f7cba.app.enclave.host          # any running risc-box deployment

# terminal 1: one SSE reader, counting events
curl -sN "$B/display" | grep --line-buffered -c '^data: '

# terminal 2, after ~15s of healthy flow: ONE unrelated request
curl -s "$B/status" -o /dev/null
```

Measured 2026-08-17 on `e64f7cba` (app `risc-box`, enclave `kryptos`):

| window | events |
|---|---|
| lone `/display`, 15s | **45** |
| the same stream, 15s after one `/status` | **0** |

Zero includes the 15-second heartbeat, so the stream is not merely starved of
bands — nothing at all is being written to that socket any more.

Second-order symptom, same cause: latency to a *new* connection collapses once
this has been happening for a while.

| idle gap before request | latency |
|---|---|
| 0s | 0.23s |
| 1s | 0.24s |
| **2s** | **18.6s** |
| 4s | 19.3s |
| 8s | 19.2s |

Warm connections stay fast; cold ones pay tens of seconds. Timings look like
SYN-retransmit backoff (1/3/7/15/31s), i.e. the app is not calling `accept()`
for long stretches — but that has not been confirmed with a packet capture, and
it should be.

## The one question to answer

**Is this the app's httpd, or the platform's gateway?** Nobody has bisected it.
Everything else below is context for that question.

### The decisive test (about fifteen minutes, needs no guest and no GPU)

Run the app locally and try the same two-curl reproduction against it.

```bash
cd ~/Projects/enclave-apps/risc-box
cargo build --release --target wasm32-wasip2

# use a wasmtime WITHOUT the set-threads patch and the PLAIN wasip2 artifact
~/Projects/wasmtime/target/debug/wasmtime run -S inherit-network \
  --env RISCBOX_CONFIG='{"title":"wedge-test"}' \
  target/wasm32-wasip2/release/risc-box.wasm
# -> [risc-box] listening on 127.0.0.1:8000
```

It serves `/console` with heartbeats while UNCONFIGURED — no S3, no kernel, no
guest needed. Verified:

```
$ curl -sN http://127.0.0.1:8000/console
: risc-box stream

event: hb
data: 1
```

Then: hold that stream open, fire `curl -s localhost:8000/status`, and watch
whether the heartbeats stop.

- **They stop** → the bug is in `src/httpd.rs` and you can fix it here.
- **They keep coming** → the app is innocent; it is the gateway/supervisor path
  (`~/Projects/enclave`, `supervisor.js` + `relay/`), and the fix is platform work.

Do this before anything else. Two prior investigations (mine included) went
wrong by reasoning about the remote symptom instead of isolating the layer.

## Ruled out, with the evidence

Do not re-litigate these.

1. **The GPU share / the `nvenc` preload.** Clean A/B on one deployment
   (`0x458a63b9`), same app, same node, DOOM running throughout:
   gpuShare 0 → 8/8 requests at 1.37-1.40s; gpuShare 0.01 → 10/10 at
   1.37-1.58s. No stalls in either arm. The preload theory is dead.
2. **Leaked SSE connections.** Real and worth fixing (see below), but not the
   cause: a tenant restart clears the connection table and the symptom returns
   immediately, with DOOM.
3. **The node, or the gateway as a whole.** A control tenant on the SAME enclave
   (`9eb4e600`, kryptos) is flat at 2.0-2.1s regardless of idle gap, while
   risc-box on the same box is 0.22s warm / 18s cold. Whatever this is, it is
   specific to this tenant or to something it does.
4. **The guest.** The emulator holds 60-116 MIPS straight through the stalls and
   bursts to 176. Instruction retirement never dips. The machine is fine; only
   the request path stalls.
5. **`realtime: false`.** It was false, it is now true, and the stutter it was
   blamed for survived the change. (It was still worth fixing for its own sake —
   DOOM ran ~7x fast and every in-guest fps number was fiction.)

## What has already shipped (mitigation, not a fix) — 0.6.26

- `src/httpd.rs`: the SSE heartbeat is a **named `hb` event** instead of a `:hb`
  comment. `EventSource` discards comments without telling the page, so a
  browser could not distinguish a still screen from a dead stream. This is what
  makes the wedge *detectable* client-side.
- `src/index.html`: a 20s stall watchdog that closes and redials the display
  stream, plus a bounded paint queue (4 deep, then drop).
- `gs-bridge/src/app.rs`: the same 20s stall timeout, chunked-transfer decoding
  (the gateway re-chunks, splitting large SSE events; the bridge silently lost
  every whole-frame band), and rectangle bands (`x`/`w`) in `screen.rs`.

Net effect: clients now recover in ≤20s instead of freezing forever. **The wedge
itself is untouched.** With one Moonlight client and nothing else talking to the
box, the bridge still redials roughly every 20 seconds, all night.

## Suspects, if it reproduces locally

`src/httpd.rs`, in `flush()`:

- `ConnState::Sse { .. } => true` — SSE conns are retained unconditionally. A
  dead peer is only dropped on a **failed write**, and a proxy in between keeps
  accepting bytes into a void, so writes never fail. One client that reconnects
  leaks a slot per reconnect, permanently. Measured: 170 leaked from a single
  gs-bridge left running ~3 hours. `broadcast()` then copies every band into all
  of their write buffers and `flush()` walks them all each turn.
  Fix direction: reap on *read* silence across N heartbeats rather than on a
  write error that cannot arrive.
- `MAX_CONNS` (384) — over the cap, new connections are dropped on the floor
  ("the proxy will retry"), which would explain cold-connect latency.
- `MAX_WBUF` (512 KiB) and `WRITE_STALL` (45s) are the only other paths that
  drop a connection.
- The loop is single-threaded: `accept()` happens once per turn, and a turn is
  `TICK_BATCH` = 400,000 guest instructions (~6ms at 67 MIPS). That should not
  produce 18s, so if it does, find out why.

## Traps that cost real time

- **`pgrep -f <pattern>` matches your own command line.** It reported Moonlight
  running when nothing was, and an earlier `pkill -f` killed the shell issuing
  it. Use `pgrep -x` / `pkill -x`.
- **Every request you make to measure this is itself a wedge trigger.** Probing
  `/status` while watching `/display` wedges the stream you are watching. A
  clean reading needs the box otherwise silent. This invalidated one A/B of mine
  outright — injected `/hid` mouse input to test a hypothesis, and the injection
  was itself the confound.
- **`/status`'s `fps` is the framebuffer SCAN rate**, not the guest's frame rate,
  and with dirty-rectangle uploads it tracks how much changed rather than how
  fast. DOOM's own counter is authoritative.
- **`deployment_logs` is owner-gated.** It needs a SIWE token from the
  deployment's owner wallet (`auth_nonce` → `personal_sign` → `auth_login`). The
  heartbeat line there carries `watchers=N/N`, which is the direct read on the
  leaked-connection count, and it was unavailable to me all evening.
- **The SET artifact will not run on a wasmtime lacking
  `wasmtime-set-threads.patch`** — it fails with "mismatch in the shared flag
  for memories". Use the plain `wasm32-wasip2` build for local work.
- **Do not leave `gs-bridge` running.** It redials every 20s because of this very
  bug, and each redial leaks a connection into the app. `pkill -x gs-bridge`.

## Also open, unrelated to the wedge

- **The desktop does not autostart.** `/etc/inittab` in `alpine/rootfs.ext2`
  carries `2:::respawn:/usr/bin/xdesktop.sh`. Busybox reads the first field as a
  tty relative to `/dev/`, so that asks for `/dev/2`. Should be `tty2`. Every
  boot lands at a bare shell prompt; `/usr/bin/xdesktop.sh` and `/usr/bin/doom`
  are present and work when run by hand. Image fix, not a wasm fix.
- **Input makes the picture subjectively smoother.** The operator reports that
  moving the mouse produces more consistent frames. Two candidate mechanisms,
  both real code paths: `input_boost` forces `parked = false` (400,000
  instructions a turn instead of 4,000), and `scan_still` resets to 0, pulling
  the framebuffer scanner back off its exponential backoff. An attempt to
  separate them by measuring the bridge's `source: N new frames/s` failed
  because the wedge dominated both arms (A: mean 2.3/s, B: mean 2.1/s, both full
  of redials). **Retry this only after the wedge is fixed** — until then the
  signal is buried.
