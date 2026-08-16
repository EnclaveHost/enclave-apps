# Tenant HTTP through app.enclave.host runs ~10x slower than the rest of the platform

This is a platform-side defect, not an app one. It caps every deployment, and
it is why a machine rendering 26 fps still looks like a slideshow from a
distance: RISC Box ships the screen as deflated dirty rectangles, ~60 KB a
frame at 1024x768, and this path allows about two of those a second.

## Measured 2026-08-16, one client (Hetzner FI enclave, ~180 ms RTT)

| target | payload | throughput |
|---|---|---|
| `9eb4e600.app.enclave.host/` (PUBLIC tenant, no auth) | 199 KB | **80 KB/s** |
| `458a63b9.app.enclave.host/a/xterm.js` (private tenant) | 283 KB | **117 KB/s** (117492, 116743, 119055 B/s) |
| `ipfs.enclave.host/ipfs/<cid>` (same platform, different service) | 3.3 MB | 1340 KB/s |
| `codeload.github.com` (off-platform control) | 5 MB | 1296 KB/s |
| the identical app binary serving the identical asset over loopback | 283 KB | **110 MB/s** |

Repro needs no credentials:

    curl -s -o /dev/null -w '%{size_download}B %{speed_download}B/s\n' \
      https://9eb4e600.app.enclave.host/
    curl -s -o /dev/null -w '%{speed_download}B/s\n' \
      https://ipfs.enclave.host/ipfs/bafybeigroxlupanlt4b5sgimbkchmcy7fm63zxzi4rlcymkzps22jqaz4a

## The clue: concurrency makes it worse

Three parallel transfers of the same 283 KB asset from one deployment:

    stream1: 9236 B/s
    stream2: 9247 B/s
    stream3: 9238 B/s      -> 27.7 KB/s aggregate, against 117 KB/s for one

A shared bandwidth cap would split ~117 three ways. A per-connection window
limit would give ~117 each. Getting 4x worse in aggregate is the signature of
serialization or head-of-line blocking — one multiplexed hop handling streams
in lockstep rather than concurrently.

Supporting arithmetic: 117 KB/s at 180 ms RTT is ~21 KB per round trip. That is
chunk-per-RTT behaviour, not a bandwidth limit. There is a prior note in the
project memory describing an "inbound request tax… ACK-lockstep signature,
fleet-wide below the app layer" — very likely the same defect seen from the
throughput side rather than the latency side.

Also measured: `connect=0.180s` but `ttfb=1.386s` on a trivial `/ping` — about
seven round trips before the first byte. That figure was taken against the
private deployment, so it includes the owner-token check; re-measure on the
public one before treating it as the baseline.

## Already ruled out

1. **The client's link.** Same machine, same minute: 1.3 MB/s from GitHub and
   from ipfs.enclave.host.
2. **The tenant app.** `httpd.rs` writes non-blocking until `EAGAIN` on every
   loop turn (~200x/s), and the same binary serving the same bytes over
   loopback does 110 MB/s single and 80-290 MB/s across three parallel streams.
3. **The private-deployment auth proxy.** A public deployment is equally slow.
4. **Client TCP receive window.** Would make parallel streams scale up.

## Where to look

- `~/Projects/enclave/supervisor.js`, `app.use("/x/:id", …)` — how the response
  body is piped to the tenant (worker backend: `/x/:id/<sub>` ->
  `/tenants/:id/<sub>`; vm backend: the app's loopback port). Look for a
  per-chunk await, a small transform buffer, or a write-without-backpressure.
- The relay in front of `app.enclave.host` (46.62.128.36 /
  2a01:4f9:c013:9b52::1), and the enclave-side tunnel for boxes advertising
  `tunnel://` endpoints.

## Experiments, in order

1. **Bisect the hops**: same asset fetched (a) inside the enclave against the
   app's loopback port, (b) from the enclave host against the supervisor's
   `/x/:id`, (c) from outside against
   `kryptos.enclave.containers.tinfoil.dev` directly, (d) from outside against
   `app.enclave.host`. (c) vs (d) alone separates relay from enclave-side proxy.
2. **A second vantage point.** Every number here is from ONE client location;
   rule out a path-specific problem between that ISP and Hetzner FI first.
3. **Packet level** on the slow hop: `ss -ti` (cwnd, retrans, rto) plus a short
   `tcpdump` settles chunk-per-RTT against loss against window.
4. **Concurrency at each hop.** Whichever turns 3x117 into 3x9 is the one.

## Hypotheses, ranked

1. The enclave↔relay leg is a single multiplexed tunnel that serializes streams
   and/or writes a chunk and waits per round trip.
2. A proxy re-chunks the body with a small buffer and awaits each write.
3. Nagle + delayed-ACK on a proxy socket (TCP_NODELAY unset on one leg).

## Done when

- One transfer from an external client reaches within ~2x of
  `ipfs.enclave.host` from that same client, and
- N parallel transfers aggregate to more than one, not less, and
- verified on both a public and a private deployment, since they take different
  paths through the auth layer.
