# The tunnel path stalls whole-hog under streaming load

Platform-side defect, measured 2026-08-20 against metal0 (tunnel-attached,
Phoenix — guest egress exits 4.15.13.178) through the us-west relay
(cc5cd64e.app.enclave.host → 5.78.85.108). It is what stands between metal0's
excellent CPU and a stall-free video stream; kryptos's direct endpoint has no
trace of it.

## The signature

One H.264 SSE stream at a modest, steady rate (2-3 Mbps → 330-500 KB/s on the
wire including base64) plus a warm-connection probe (`GET /status` every
100 ms on its own persistent TLS connection):

| condition | probe p50 | probe p90 | probe >500 ms |
|---|---|---|---|
| idle (no stream) | 389 ms | 394 ms | 1 of 58 |
| while streaming | 400 ms | **1008 ms** | **26 of 131** |

The stream itself shows the same windows: ~1 s holes with ZERO events, every
~10-20 s, followed by a catch-up burst (the bytes were queued, not lost).
Lowering the stream to 2 Mbps does not remove them. The client's own uplink
pinged 8.8.8.8 240/240 with nothing over 60 ms through the worst of it, and the
app's `videoFps` gauge shows production pausing only AFTER the socket stops
draining (the SSE backlog gate at 192 KB doing its job) — so the stall is
below the app and beyond the client.

Two more data points that localize it:

- A warm request through this path costs 389 ms IDLE — flat, so it is a fixed
  tax, not congestion. The direct kryptos endpoint answers ~100 ms cold.
- The probe rides a DIFFERENT TCP connection than the stream and freezes in
  the same windows: whatever stalls, stalls the whole tunnel, not one stream.

## Suspects, in the order I would look

1. **The supervisor/wasm-manager event loop on the box** — everything proxied
   shares it. A periodic synchronous chunk of work that scales with traffic
   fits the "only under load" shape; `saveStateNow`'s `writeFileSync` ticks
   every 2 s when dirty (supervisor.js:1901), and the billing/lease beats run
   at ~15 s, which rhymes with the observed period.
2. **The relay↔box tunnel transport** — if it is one multiplexed TCP, one
   loss puts every stream behind an in-order hole (HOL); the fix shape is
   per-stream connections or a window bump.
3. **The relay's HTTP proxy layer** (us-west, node) — GC or a buffer-copy
   path that only hurts at streaming rates.

The 389 ms idle floor is its own finding: metal0 is ~30-40 ms from the relay
and the relay ~40 ms from this client, so ~310 ms is being spent inside the
machinery per request. The old "inbound request tax" note (ACK-lockstep,
below the app layer) was very likely this same thing seen from the side.

## How to reproduce in five minutes

1. Any deployment on a tunnel box; api_key set; owner app-token in hand.
2. Stream: `curl -sN ".../video?codec=h264&kbps=3000" > /dev/null`.
3. Probe (same box, separate connection): the warm-probe.py pattern — one
   persistent HTTPS connection, `GET /status` every 100 ms, log latencies.
4. Watch stalls >500 ms cluster only while (2) runs.
