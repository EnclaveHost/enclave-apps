# backchannel — e2e-encrypted ephemeral chat rooms as an Enclave service app

The server is blind and you can check. Every message that reaches this
process is AES-256-GCM ciphertext sealed in a browser; the room key rides
the invite link's URL *fragment* and never crosses the wire; even the
nicknames travel **inside** the plaintext. The enclave relays and rings what
it cannot read — and because it runs in a **hardware-attested TEE** from a
build reproducible from this source via the on-chain catalog, "blind relay"
is a property you verify, not a promise you extend.

```
alice's browser                  the enclave                 everyone in the room
  AES-256-GCM encrypt   ──POST──>  ring[200] per room  ──SSE──>  decrypt with the key
  key from the #fragment           in RAM, swept after           from the same #fragment
  nick inside the plaintext        2 idle hours
```

## The trust math

- **The key never travels.** It rides the invite link's URL *fragment*
  (`https://…/r/<id>#k=<key>`), which browsers do not send in requests. The
  server relays ciphertext it cannot read, in a room the *client* named.
- **Even "who's talking" is ciphertext.** The nickname is a field inside the
  encrypted JSON, not a header. The server sees blobs, sizes and timing —
  the irreducible metadata of any relay — and nothing else.
- **Rooms are RAM and they forget.** History is a 200-message ring per room;
  older messages fall off as new ones arrive, and a room dies two hours
  after its last message. The platform gives service apps no filesystem.
- **Misses are uniform.** A room that never existed and a room that died are
  the same 404 — holding a dead invite proves nothing about what was said.
- **Presence is a count.** Subscribers learn how many streams are open,
  never who is behind them. Logs carry counts, never ids or blobs.

## Features

- One-click room: the page generates the id and the 32-byte key, the invite
  link carries both, and the server is told only the id.
- Live rooms over SSE: messages fan out as `msg` events; a `present` event
  keeps the "N here" count honest, ticked at most every 5 seconds.
- Nicknames per room (stored only in your browser), deterministic per-nick
  colors, optimistic sends settled by the relay's own echo.
- Caps: 8 KiB ciphertext per message, 2 000 rooms, 64 MiB total; at capacity
  it says so rather than evicting someone else's room.

## API

Bodies are opaque base64url blobs or `k=v&` forms; JSON is emit-only. The
server never parses JSON, never generates randomness, never picks an id.

| route | in | out |
|---|---|---|
| `POST /api/rooms` | header `x-room-id` | `{ok}` — 409 if taken, 507 at capacity |
| `POST /api/msg` | `id=<room>&blob=<b64u>` | `{ok, n}` + SSE `msg` fan-out |
| `GET /api/history?id=` | — | `{seq, msgs[]}` oldest first, the ring's tail |
| `GET /api/stream?id=` | — | SSE `msg` / `present` events on topic `<id>` |
| `GET /api/stats` | — | counts only |
| `GET /` , `GET /r/<id>` UI · `GET /ping` liveness | | |

## Design notes

Zero dependencies: `src/httpd.rs` is the suite's hand-rolled HTTP/1.1 + SSE
engine (one non-blocking event loop, the nanircd shape — wasip2 has no
threads), and the whole UI is one embedded HTML file with inline WebCrypto.
The one platform rule, as ever: **read `ENCLAVE_PORTS`, bind the actual
port** — the deployment's `http:` entry is served at its origin by the
enclave's in-TEE TLS proxy.

## Build & test

```bash
rustup target add wasm32-wasip2        # or your distro's wasip2 std
cargo build --release --target wasm32-wasip2
# → target/wasm32-wasip2/release/backchannel.wasm  (~190 KB component)

wasmtime run -Scli -Stcp -Sinherit-network -Sallow-ip-name-lookup \
  --env 'ENCLAVE_PORTS=http:8080=18084' \
  target/wasm32-wasip2/release/backchannel.wasm
# then open http://127.0.0.1:18084
```

## Deploy on enclave.host

Publish the component (see the repo README / `guide` topic "publish"), then
deploy CPU-only — the minimum share is plenty:

```
enclave deploy <publisher>/backchannel --cpu 0.01 --fund 2
```

Before trusting a deployment with a real conversation, verify its
attestation (guide topic "attestation"): the whole point is that you don't
have to take anyone's word — including ours — for what's relaying you.
