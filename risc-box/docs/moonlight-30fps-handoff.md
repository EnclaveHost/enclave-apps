# Stable 30 fps through Moonlight: what carries it, and what almost did

Goal (2026-08-20): FreeDoom on the shipped 1024x768 Alpine desktop, streamed
to a real Moonlight client at a STABLE 30 fps, with cursor and keyboard input
flowing the whole time. Everything below is measured; the negative results are
half the value.

## The number

On kryptos's direct endpoint, app 0.7.15 (risc-box 0.6.41), 3 Mbps, a
90-second driven-gameplay session first, then the long soak (see the addendum
at the end for the 30-minute figures):

    fps per second: min 38  p5 39  p10 39  median 40  mean 40.0  max 43
    seconds below 30: 0 of 86      seconds below 24: 0 of 86

Every frame on the wire is app-encoded H.264 repacketized untouched, so the
client's decode rate IS the app's frame rate — no NVENC duplication inflating
the count (the old bridge encoded its mirror at 60 fps regardless; its "18
fps" was 1.5 fresh frames/s of mirror plus 58 duplicates). The session is real
play: the harness navigates the menu into a new game and holds
move/turn/fire/use plus a ~20 Hz mouse sweep for the whole soak — the decoded
stream shows the pickup messages and the health counter paying for it.

## The architecture

    guest paints 1024x768 ── capture (0.1-1.2 ms memcpy, emulator thread)
      └─> SET worker: BGRX→I420 (one fused pass) → minih264 → H.264 AU
            └─> GET /video?codec=h264&kbps=N   (base64 SSE, one event per AU)
                  └─> gs-bridge --frames h264: REPACKETIZE ONLY → RTP+FEC
                        └─> moonlight client decodes; input returns via
                            ENet → POST /hid → virtio-input

Codec choice was forced, not preferred: DOOM produces 6-10 MiB/s of deflated
lossless bands against ~1 MiB/s links (the client saw 1.5 fresh frames/s), and
rav1e at this size runs 2.9 fps in the wasm. minih264 (vendored, CC0, one
marked one-line wasm patch) encodes 768p in 2.7 ms native — 374 fps on the
bench — and 1-3 ms/frame in the wasm on metal0, 23 ms on kryptos. AV1 stays
the browser codec; `?codec=h264` is the Moonlight one.

## The five cliffs between "it streams" and "it holds 40"

1. **A video-only watcher parked the capture at 10 fps.** Stillness is derived
   from the band diff, bands are not computed for a video watcher, so
   `fb_still` only ever grew and the scan backed off to its 100 ms ceiling —
   measured as exactly 10.0 fps. Video watching now pins the backoff open.
2. **The join keyframe died before the client could hear it.** The bridge
   opens /video at RTSP ANNOUNCE, the app leads with an IDR, but the client's
   video port is only learned from its first ping — so the IDR went nowhere
   and the client waited ~15 s (the then-GOP) for the next one: a 24-second
   dead start. The bridge now orders an IDR the moment the peer appears.
3. **Elapsed-interval pacing quantizes to the capture clock.** "Encode if
   ≥25 ms since the last" on jobs arriving every 16 ms means every OTHER job:
   32 ms, 31 fps designed, 27-28 delivered. An absolute deadline advanced by
   the interval absorbs the beat and delivers the true 40.
4. **Unbounded keyframes are self-sustaining stalls.** minih264 spends ~125 KB
   on a 768p intra frame at qp_min 10 — ~167 KB in base64, nearly the whole
   192 KB SSE production gate in one event. Worse, every starve-recovery mints
   a fresh encoder, whose first frame is another 125 KB: the pipeline
   oscillated (periodic 0-fps seconds, IDR bursts right after). Keys are now
   demand-only (gop = 0; join and /video-key), qp_min 26 with a 4x P budget —
   a bounded, softer keyframe that P-frames refine within ~100 ms.
5. **One job in flight halves a slow worker.** `inflight == 0` serialized the
   23 ms kryptos encode with the capture handoff: 28 fps on hardware whose
   encoder does 40+. Depth 2 keeps the worker saturated — max(encode, capture)
   instead of their sum — for one frame of staleness.

## The node tradeoff, measured

| | metal0 (Phoenix, tunnel-only) | kryptos (direct endpoint) |
|---|---|---|
| guest render (FreeDoom demo) | 45-70 fps | slower cores (~0.6x) |
| worker encode, 768p | 1-3 ms | 23-27 ms |
| warm request, idle | 389 ms (p50, flat) | ~100 ms |
| warm request, while streaming | p90 > 1 s, 26/131 stalled | flat |
| stream profile | median 40, but 0-fps holes every ~10 s | min 38, no holes |

metal0's holes are the platform's, not the app's: a warm probe on a separate
connection freezes in the same windows (whole-tunnel shared fate), idle is
clean, and my own uplink pinged 240/240 flat through it all. See
`tunnel-stall-handoff.md`. Until that is fixed, the direct endpoint wins even
with slower silicon; when it IS fixed, metal0's 45-70 fps guest makes nearly
every encoded frame a distinct game frame at 40 fps.

## Private data path, since every dev deploy is private

Session tokens are per-enclave (each box honors only its own kid):
`/v1/auth/nonce?enclave=X` + `login?enclave=X` pins the mint. Trade the
session for a deployment-scoped app token (`POST
/v1/deployments/<id>/app-token`) and send it as the `enclave_app` COOKIE —
the relay consumes Authorization, the cookie survives everywhere. The bridge
grew `--app-cookie`, and `--app` accepts a path-prefixed URL
(`https://kryptos.enclave.containers.tinfoil.dev/x/<id>`), which is how a
tenant is addressed on a direct endpoint.

## What would make it better still

- **Binary framing for /video.** base64-over-SSE taxes the wire 33%; a
  length-prefixed chunked body would carry 4 Mbps in the bytes 3 costs today.
- **wasm SIMD128 for minih264.** The C is plain scalar; kryptos's 23 ms/frame
  is the ceiling that matters there.
- **The metal0 tunnel stall** — the platform fix that unlocks the fast node.
- **In-enclave NVENC** (`nvenc:` wasmtime branch exists) — the H200 path, for
  when the stream should never leave the enclave unencoded anyway.
