# dead-drop — burn-after-reading secrets as an Enclave service app

A one-page secret-sharing service in the PrivateBin/One-Time-Secret family,
with the part those can't offer: the server that counts your reads and erases
the ciphertext runs inside a **hardware-attested TEE**, from a build you can
reproduce from this source via the on-chain catalog. "We can't read it and we
don't keep it" stops being a promise and becomes a property.

```
sender's browser                     the enclave                    recipient's browser
  AES-256-GCM encrypt   ──POST──>   {id -> blob, reads, ttl}   ──POST /take──>  decrypt
  key -> link #fragment              in RAM, erased at 0/TTL          key from #fragment
```

## The trust math

- **The key never travels.** It rides the link's URL *fragment*
  (`https://…/#<id>.<key>`), which browsers do not send in requests. The
  server stores ciphertext it cannot read, under an id the *client* chose.
- **Exactly-N reveals is atomic.** wasip2 has no threads; take-decrement-erase
  is one uninterruptible step in one process. Two people racing the last read
  can't both win.
- **Misses are uniform.** Unknown, expired, burned and consumed ids are the
  same 404 — holding a dead link proves nothing about what existed.
- **Nothing touches a disk.** The platform gives service apps no filesystem;
  state is enclave RAM and dies with the deployment. Logs carry counts, never
  ids or blobs.

## Features

- 1 / 3 / 10 / 25 reveals per drop; 1 hour / 24 hour / 7 day expiry.
- **Sender burn link**: created alongside the share link (`#!<id>.<token>`);
  presenting the token (checked against its SHA-256) erases the drop early.
- Live public stats (`/api/stats`) — counts only, by construction.
- Caps: 64 KiB plaintext, 20 000 drops, 48 MiB total; at capacity it says so
  rather than evicting someone's secret.

## API

Bodies are opaque base64url blobs or `k=v&` forms; JSON is emit-only. The
server never parses JSON, never generates randomness, never picks an id.

| route | in | out |
|---|---|---|
| `POST /api/drop` | headers `x-drop-id`, `x-reads`, `x-ttl`, `x-burn-hash?`; body = blob | `{ok, expires_at}` |
| `POST /api/take` | `id=<id>` | `{blob, reads_left}` — decrements, erases at 0 |
| `POST /api/peek` | `id=<id>` | `{reads_left, expires_in, size}` — no consume |
| `POST /api/burn` | `id=<id>&token=<secret>` | `{ok}` if SHA-256 matches |
| `GET /api/stats` | — | counts only |
| `GET /` UI · `GET /ping` liveness | | |

## Design notes

Zero dependencies: `src/httpd.rs` is the suite's hand-rolled HTTP/1.1 + SSE
engine (one non-blocking event loop, the nanircd shape — wasip2 has no
threads), `src/sha256.rs` is FIPS 180-4 by hand, and the whole UI is one
embedded HTML file with inline WebCrypto. The one platform rule, as ever:
**read `ENCLAVE_PORTS`, bind the actual port** — the deployment's `http:`
entry is served at its origin by the enclave's in-TEE TLS proxy.

## Build & test

```bash
rustup target add wasm32-wasip2        # or your distro's wasip2 std
cargo build --release --target wasm32-wasip2
# → target/wasm32-wasip2/release/dead-drop.wasm  (~190 KB component)

wasmtime run -Scli -Stcp -Sinherit-network -Sallow-ip-name-lookup \
  --env 'ENCLAVE_PORTS=http:8080=18080' \
  target/wasm32-wasip2/release/dead-drop.wasm
# then open http://127.0.0.1:18080
```

## Deploy on enclave.host

Publish the component (see the repo README / `guide` topic "publish"), then
deploy CPU-only — the minimum share is plenty:

```
enclave deploy <publisher>/dead-drop --cpu 0.01 --fund 2
```

Before trusting a deployment with real secrets, verify its attestation
(guide topic "attestation"): the whole point is that you don't have to take
anyone's word — including ours — for what's running.
