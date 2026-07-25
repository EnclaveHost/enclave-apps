# hookbin — a webhook/request inspector as an Enclave service app

A requestbin-style capture service: point any webhook or HTTP client at your
capture URL and inspect everything it sent — live — with the part those
services can't offer: the server that reads your traffic runs inside a
**hardware-attested TEE**, from a build you can reproduce from this source
via the on-chain catalog. Webhook payloads carry Stripe events, OAuth
tokens, PII; "we don't look and we don't keep it" stops being a promise and
becomes a property.

```
webhook sender                        the enclave                      your browser
  ANY /b/<token>       ──────>   {token -> ring of 200}    ──SSE──>   /i/<token>, live
  gets your configured reply       captures in RAM only               headers, body, curl
```

## The trust math

- **The payloads never leave the enclave.** Captures are held in enclave
  RAM, served only back over the deployment's in-TEE TLS proxy, and erased
  on delete, after 24 h idle, or with the deployment. The platform gives
  service apps no filesystem — there is nothing to subpoena.
- **Tokens are client-chosen and never logged.** The server generates no
  randomness, picks no names, and prints counts only. Misses of every kind
  — bad token, unknown, expired, deleted — are the same 404.
- **Bounded on every axis.** 500 bins, 200 captures per bin (a ring, oldest
  dropped), 64 KiB stored per body (original length kept), 64 MiB stored
  total, 24 h idle expiry. Capacity pressure evicts the least-recently-active
  bin, so the debugging happening right now always has room.
- **What runs is what you read.** Zero dependencies, one process, one event
  loop; the attestation covers this exact source.

## Features

- Capture **any method** at `/b/<token>` or any subpath — target with query
  string, headers, and body recorded as received (bodies stored to 64 KiB,
  original length and a truncation flag kept).
- Live inspector: SSE stream with a pulsing status dot, method badges,
  relative timestamps, pretty-printed JSON, hex dump for binary bodies,
  per-request **copy as curl**.
- Configurable reply per bin — status, content-type, body (default
  `200 {"ok":true}`) — so real callers can be pointed at it mid-debug.
- Self-healing links: opening `/i/<token>` for a missing bin offers to
  create that token on the spot.
- Live public stats (`/api/stats`) — counts only, by construction.

## API

Tokens match `^[a-z0-9][a-z0-9-]{7,31}$` and are chosen by the client;
JSON is emit-only. Capture objects are one shape everywhere:
`{n, ts, method, target, headers: [[k,v],…], body_b64, truncated, len}`.

| route | in | out |
|---|---|---|
| `POST /api/bins` | header `x-bin-id` | `{ok}` \| 409 `{error:"exists"}` |
| `ANY /b/<token>[/…]` | anything at all | the bin's configured reply |
| `GET /api/bins/<token>/requests` | — | JSON array, oldest first |
| `POST /api/bins/<token>/response` | headers `x-status` (100–599), `x-ct`; body ≤ 8 KiB | `{ok}` |
| `POST /api/bins/<token>/clear` | — | `{ok}` — empties the ring, keeps the bin |
| `DELETE /api/bins/<token>` | — | `{ok}` |
| `GET /api/stream/<token>` | SSE | one `event: req` per capture, same JSON |
| `GET /api/stats` | — | counts only |
| `GET /` and `/i/<token>` UI · `GET /ping` liveness | | |

## Design notes

Zero dependencies: `src/httpd.rs` is the suite's hand-rolled HTTP/1.1 + SSE
engine (one non-blocking event loop, the nanircd shape — wasip2 has no
threads), stamped in unchanged; base64 is ~20 lines of std in `main.rs`; the
whole UI is one embedded HTML file that builds capture rows with
`textContent` only — captures are hostile input. SSE frames and list items
share one JSON emitter, so the client renders both paths with one function.
Two engine-inherited edges worth knowing: chunked request bodies get a 501
(senders must use `content-length`), and bodies over 256 KiB on the wire get
a 413 (the first 64 KiB of accepted bodies is what's stored). The one
platform rule, as ever: **read `ENCLAVE_PORTS`, bind the actual port** — the
deployment's `http:` entry is served at its origin by the enclave's in-TEE
TLS proxy.

## Build & test

```bash
rustup target add wasm32-wasip2        # or your distro's wasip2 std
cargo build --release --target wasm32-wasip2
# → target/wasm32-wasip2/release/hookbin.wasm  (~200 KB component)

wasmtime run -Scli -Stcp -Sinherit-network -Sallow-ip-name-lookup \
  --env 'ENCLAVE_PORTS=http:8080=18081' \
  target/wasm32-wasip2/release/hookbin.wasm
# then open http://127.0.0.1:18081
```

## Deploy on enclave.host

Publish the component (see the repo README / `guide` topic "publish"), then
deploy CPU-only — the minimum share is plenty:

```
enclave deploy <publisher>/hookbin --cpu 0.01 --fund 2
```

Before pointing production webhooks at a deployment, verify its attestation
(guide topic "attestation"): the whole point is that you don't have to take
anyone's word — including ours — for who can read what you capture.
