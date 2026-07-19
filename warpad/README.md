# warpad — an e2e-encrypted shared scratchpad as an Enclave service app

**No history, anywhere.** One shared text pad per link — notes for an
incident bridge, a negotiation, an interview panel — in the CryptPad family,
minus the part that gets subpoenaed: the document only ever exists
server-side as **one** AES-256-GCM ciphertext, and every save replaces it
whole. There is no revision log, no journal, no undo buffer — yesterday's
draft provably no longer exists the moment today's lands. And the process
doing the replacing runs inside a **hardware-attested TEE**, from a build
you can reproduce from this source via the on-chain catalog, so "we hold
one blob we can't read" is a property you verify, not a promise you extend.

```
alice's browser                    the enclave                    everyone on the pad
  AES-256-GCM the WHOLE doc ──POST──> {id -> blob, version}  ──SSE──> decrypt, keep typing
  key from the #fragment             ONE ciphertext in RAM,           key from the same #fragment
  ~1s after she stops typing         REPLACED on every save
```

## The trust math

- **The key never travels.** It rides the link's URL *fragment*
  (`https://…/p/<id>#k=<key>`), which browsers do not send in requests. The
  server versions ciphertext it cannot read, in a pad the *client* named.
- **Replacement is the retention policy.** The pad is a single blob; a save
  overwrites it in place and bumps a version counter. Old versions aren't
  deleted on a schedule — they never exist at all. There is nothing to
  subpoena, leak, or diff.
- **Conflicts are the client's job.** Saves carry the base version; a stale
  save gets a 409 with the current state and the *browser* rebases (a
  line-range merge, with an undo offered when edits collide). The server
  can't merge what it can't read.
- **Pads are RAM and they forget.** A pad dies 24 hours after its last
  write; the platform gives service apps no filesystem. Misses of every
  kind are the same 404, and logs carry counts, never ids or blobs.
- **Presence is a count.** Subscribers learn how many streams are open,
  never who is behind them. It is self-reported liveness, not proof: the
  page beacons `/api/leave` on pagehide so a closed tab drops out of the
  count instantly, and the engine reaps write-stalled peers as a backstop
  when a beacon never arrives. Between those two, a stale "N here" is
  possible but short-lived — a count of open sockets is all it ever claims
  to be.

## Features

- One-click pad: the page generates the id and the 32-byte key, the link
  carries both, and the server is told only the id.
- Live sync over SSE: every save fans out as a `doc` event; everyone on the
  pad sees edits within a second, with debounced (~800 ms) whole-document
  saves and cursor-preserving remote updates.
- Optimistic concurrency with client-side rebase: disjoint line edits merge
  cleanly; overlapping ones prefer your text and offer theirs back on a
  dismissible warning bar.
- Caps: 256 KiB ciphertext per pad, 1 000 pads, 64 MiB total; at capacity
  it says so rather than evicting someone's pad.

## API

Bodies are opaque base64url blobs or `k=v&` forms; JSON is emit-only. The
server never parses JSON, never generates randomness, never picks an id.

| route | in | out |
|---|---|---|
| `POST /api/pads` | header `x-pad-id` | `{ok}` — 409 if taken, 507 at capacity |
| `POST /api/save` | `id=<pad>&v=<base>&blob=<b64u>` | `{ok, version}` — 409 `{version, blob}` on a stale base; empty blob clears |
| `GET /api/pad?id=` | — | `{version, blob, expires_in}` |
| `GET /api/stream?id=&s=` | — | SSE `doc` / `present` events on topic `<id>`; the first event is the current document. `s=` is an opaque per-tab stream tag |
| `POST /api/leave` | `id=<pad>&s=<tag>` | `{ok}` always — closes that stream and corrects presence at once; fire-and-forget, so it can't probe pad ids |
| `GET /api/stats` | — | counts only |
| `GET /` , `GET /p/<id>` UI · `GET /ping` liveness | | |

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
# → target/wasm32-wasip2/release/warpad.wasm  (~190 KB component)

wasmtime run -Scli -Stcp -Sinherit-network -Sallow-ip-name-lookup \
  --env 'ENCLAVE_PORTS=http:8080=18086' \
  target/wasm32-wasip2/release/warpad.wasm
# then open http://127.0.0.1:18086
```

## Deploy on enclave.host

Publish the component (see the repo README / `guide` topic "publish"), then
deploy CPU-only — the minimum share is plenty:

```
enclave deploy <publisher>/warpad --cpu 0.01 --fund 2
```

Before trusting a deployment with a real draft, verify its attestation
(guide topic "attestation"): the whole point is that you don't have to take
anyone's word — including ours — for what's holding the only copy.
