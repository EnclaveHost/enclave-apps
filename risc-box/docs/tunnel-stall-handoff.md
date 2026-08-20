# The tenant socket freeze: engine-layer, SET-amplified

Rewritten 2026-08-20 (third pass tonight — each earlier conclusion was
falsified by a sharper probe; the ladder below is the whole story and each
rung is measured, so start at the bottom).

## The defect

A tenant streaming SSE (any stream: H.264 /video or deflate /display) while
serving a handful of concurrent requests intermittently freezes — ALL of that
tenant's TCP at once, 0.5-14 s per episode, clustered — while the guest keeps
executing at full speed. A Moonlight player feels it as the game freezing
whenever they do anything that generates requests (mouse, keys).

## The exoneration ladder (measure in this order next time)

1. **The client's network**: ping 8.8.8.8 through the worst of it — 240/240,
   nothing over 60 ms.
2. **The path**: ICMP to the serving host itself, flat, zero loss, while its
   TCP stalled (kryptos 69.46.85.219, metal0 via 5.78.85.108).
3. **The relay/tunnel**: same freezes on kryptos's DIRECT endpoint — no
   relay, no tunnel. (metal0's relay path adds a fixed 389 ms/request tax and
   its own exposure, but it is not the cause.)
4. **The ingress shim**: /.well-known/tinfoil-attestation flat (p90 109 ms,
   0 stalls) during tenant stalls.
5. **The supervisor's event loop**: its own /v1/pricing flat (p90 116 ms,
   0 stalls) during tenant stalls.
6. **The box / other tenants**: a second tenant on the same box answered
   250/250 clean, interleaved with 16 stalls on this one. Per-tenant, not
   box-wide.
7. **The app's own loop**: per-turn telemetry (0.6.43: turnMaxMs/turnMax in
   /status, SLOW TURN log lines) — worst turn 11-13 ms while a warm probe on
   the same app waited 4.8-14 s. The Rust loop never stopped; its bytes did.

What remains between a healthy guest loop and a healthy supervisor is the
per-tenant wasmtime host: the wasip2 socket/stream layer of the fleet engine.

## The differential that names the suspect

Same deployment, same box, same load pattern (one SSE stream + ~7/s warm
probes):

| build | stalls / probes | worst |
|---|---|---|
| SET (shared-everything-threads) | 5 / 162 and 16 / 178 across runs | **14.2 s** |
| plain wasip2 | 0 / 243 | 317 ms |

Doubling the load on the plain build (two streams + 8/s probes) does
reproduce the class — 9 / 361, worst 2.1 s, loop still 12 ms — so the base
engine's socket layer is implicated too, but the SET patch drops the trigger
threshold and raises the ceiling by an order of magnitude.

## Where to look (engine repo)

- The CLI p2 socket/stream host path under `-S inherit-network`: what pumps
  output-stream flushes and pollables when one writer streams continuously
  and several keep-alive connections poll — a starved driver or a lock
  convoy here freezes every socket of the instance while guest code runs on,
  which is exactly the signature.
- `wasmtime-set-threads.patch`: the per-thread fd-namespace layer sits on
  that same path on every fd op; whatever it serializes, both the app thread
  and the worker thread cross it. The 10x severity delta is the tell.
- Reproduce anywhere: risc-box 0.7.17 (SET) vs 0.7.18-plain are published
  side by side; stream `/video?codec=h264` and hammer `/status` at ~7/s on a
  warm connection; stalls >500 ms appear within a minute on SET.

## What this is NOT (also measured tonight, all fixed app/bridge-side)

Encoder overshoot at motion (fixed: MB-level RC), pointer event storms
(fixed: coalesced cursor state), the /hid-stream body-buffering blackhole
(fixed: loopback-only), the streamPacketIndex u16 wrap (fixed), keyframe
burst oscillation (fixed: gop=0, bounded IDRs). After all of those the app
produces a flat 40 fps through 250 Hz mouse input — this engine freeze is
the only thing left between the chain and an uninterrupted stable 30+.
