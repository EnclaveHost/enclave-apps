# handoff — files through an attested enclave

A file wormhole with a warehouse in the middle: magic-wormhole ergonomics,
but the relay is a **hardware-attested TEE** that holds only ciphertext it
cannot name, and the "we delete it after delivery" is a property of code you
can reproduce from this source via the on-chain catalog — not a promise.

```
sender's browser                      the enclave                     receiver's browser
  chunk · AES-256-GCM   ──POST──>   {id -> ct chunks, reads}  ──GET──>   decrypt · assemble
  key -> link #fragment              in RAM, erased on delivery          key from #fragment
```

## The trust math

- **The key never travels.** It rides the link's URL *fragment*; the server
  stores ciphertext under an id the *client* chose. Even the **filename**
  travels only inside an encrypted manifest — the enclave's entire knowledge
  of a handoff is an opaque id, a chunk count, byte totals, timestamps.
- **Downloads count on completion, not curiosity.** A claim consumes a
  download only when its final chunk has been served — a dropped connection
  doesn't burn the transfer. At zero downloads, TTL, or the sender's burn
  link, the only copy is erased.
- **Raw bytes on the wire.** Chunks travel as `application/octet-stream`
  both directions — no base64 bloat in enclave RAM.
- **Misses are uniform.** Unknown, expired, burned and delivered ids are the
  same 404.

## Limits

64 MB per file (256 × 256 KiB chunks), 192 MB held at once, 1/3/5 downloads,
1 hour / 24 hour / 3 day expiry. Stalled uploads die after 15 idle minutes;
claims expire after 15. At capacity it says so rather than evicting
someone's file.

## API

Headers + `k=v&` forms in, emit-only JSON out; chunk bodies are raw bytes.

| route | in | out |
|---|---|---|
| `POST /api/new` | headers `x-drop-id`, `x-chunks`, `x-bytes`, `x-reads`, `x-ttl`, `x-burn-hash?`; body = encrypted manifest | `{ok, expires_at}` |
| `POST /api/put` | headers `x-drop-id`, `x-chunk`; body = chunk ciphertext | `{ok, have, complete}` |
| `GET /api/meta?id=` | — | `{chunks, bytes, complete, reads_left, expires_in, manifest}` |
| `POST /api/claim` | `id=&t=<client token>` | `{ok, chunks}` — completion counts it |
| `GET /api/chunk?id=&i=&t=` | — | raw ciphertext bytes |
| `POST /api/burn` | `id=&token=` | `{ok}` if SHA-256 matches |
| `GET /api/stats` · `GET /` UI · `GET /ping` | | |

## Build & test

```bash
cargo build --release --target wasm32-wasip2
wasmtime run -Scli -Stcp -Sinherit-network -Sallow-ip-name-lookup \
  --env 'ENCLAVE_PORTS=http:8080=18085' \
  target/wasm32-wasip2/release/handoff.wasm
# then open http://127.0.0.1:18085
```

Design notes are dead-drop's: zero dependencies, the suite's hand-rolled
HTTP/1.1 + SSE engine (`src/httpd.rs`), hand-rolled SHA-256 for burn tokens,
one embedded page of inline WebCrypto. Deploy CPU-only at the minimum share;
attest before handing it anything real.
