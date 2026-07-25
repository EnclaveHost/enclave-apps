# pixelboard — a shared pixel canvas as an Enclave service app

A one-page collaborative canvas in the r/place family, with the part those
can't offer: the server that meters the cooldown and holds the artwork runs
inside a **hardware-attested TEE**, from a build you can reproduce from this
source via the on-chain catalog. "Everyone plays by the same rules" stops
being a moderation policy and becomes a property — and when the deployment's
funding ends, the canvas provably ceases to exist. Fun, ephemeral, communal.

```
every browser                        the enclave                      every browser
  click a cell   ──POST /px──>   board[16384] + cooldowns   ──SSE px──>  the same canvas,
  16 colors, 1 per 2s             in RAM, one process                    live, for everyone
```

## The trust math

- **The rules are the build.** The 2-second cooldown, the palette, the board
  size — all attested code. Nobody gets a faster brush, nobody edits the
  board out-of-band, not even the operator.
- **Placement is atomic.** wasip2 has no threads; check-cooldown-paint-stamp
  is one uninterruptible step in one process. The rate limit cannot be raced.
- **Sessions are anonymous.** The browser mints its own opaque random token;
  the server never issues identity, holds tokens only to meter the cooldown,
  and sweeps them a minute after the last placement.
- **Nothing touches a disk.** The platform gives service apps no filesystem;
  the artwork is enclave RAM and dies with the deployment — no snapshot, no
  backup, no afterlife. Logs carry counts, never tokens.
- **"Painting now" is self-reported liveness.** It counts open SSE sockets,
  nothing more. The page beacons `/api/leave` on pagehide so a closed tab
  leaves the count instantly, and the engine reaps write-stalled peers as a
  backstop when a beacon never arrives — a proxy hop will otherwise hold a
  dead tab's socket open for a long time. The stream tag in that beacon is
  a separate per-tab token: the cooldown session token is per-person and
  never becomes stream identity.

## Features

- 128×128 board, 16 fixed colors, one placement per session every 2 seconds.
- Live SSE delta stream, batched at ~200 ms, with a version counter so
  clients detect gaps and resync from `/api/board`.
- Full-viewport canvas UI: wheel/pinch zoom, drag pan, hover preview,
  cooldown ring on the selected swatch, live "painting now" counts.
- Live public stats (`/api/stats`) — counts only, by construction.

## API

Bodies are `k=v&` forms; JSON is emit-only. The server never parses JSON,
never generates randomness, never issues a session.

| route | in | out |
|---|---|---|
| `GET /api/board` | — | `{w, h, palette, placed, version, painting, board}` — board is base64 of 16384 palette indices |
| `POST /api/px` | `i=<0..16383>&c=<0..15>&s=<session>` | `{ok, wait_ms}` · on cooldown `429 {wait_ms}` |
| `GET /api/stream?s=` | — | SSE: `px` = `{v, d:[[i,c],…]}` delta batches · `n` = `{painting, placed}`. `s=` is an opaque per-tab stream tag |
| `POST /api/leave` | `s=<tag>` | `{ok}` always — closes that stream and re-broadcasts `n` at once; `id=` is accepted and ignored (one board) |
| `GET /api/stats` | — | counts only |
| `GET /` UI · `GET /ping` liveness | | |

## Design notes

Zero dependencies: `src/httpd.rs` is the suite's hand-rolled HTTP/1.1 + SSE
engine (one non-blocking event loop, the nanircd shape — wasip2 has no
threads), base64 is ~20 lines of std, and the whole UI is one embedded HTML
file drawing to a single `<canvas>`. The one platform rule, as ever: **read
`ENCLAVE_PORTS`, bind the actual port** — the deployment's `http:` entry is
served at its origin by the enclave's in-TEE TLS proxy.

## Build & test

```bash
rustup target add wasm32-wasip2        # or your distro's wasip2 std
cargo build --release --target wasm32-wasip2
# → target/wasm32-wasip2/release/pixelboard.wasm  (~180 KB component)

wasmtime run -Scli -Stcp -Sinherit-network -Sallow-ip-name-lookup \
  --env 'ENCLAVE_PORTS=http:8080=18083' \
  target/wasm32-wasip2/release/pixelboard.wasm
# then open http://127.0.0.1:18083
```

## Deploy on enclave.host

Publish the component (see the repo README / `guide` topic "publish"), then
deploy CPU-only — the minimum share is plenty:

```
enclave deploy <publisher>/pixelboard --cpu 0.01 --fund 2
```

Verify the deployment's attestation (guide topic "attestation") before you
trust the rules — the whole point is that you don't have to take anyone's
word, including ours, for what's enforcing the cooldown. And know what you're
signing up for: when the funding runs out, the deployment is destroyed and
the canvas with it. That's not a limitation; it's the art.
