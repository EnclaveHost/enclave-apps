# ballot — anonymous sealed polls as an Enclave service app

A one-page polling service in the Strawpoll family, with the part those can't
offer: **sealed** mode, where nobody — voters, bystanders, the platform
operator, *the poll's own creator* — can see the tally until the poll is
closed. Any ordinary server can promise that; its operator can also open a
debugger. Here the counts exist only in the RAM of a **hardware-attested
TEE**, running a build you can reproduce from this source via the on-chain
catalog, and the code that holds them provably refuses to answer early.
No mid-vote peeking, no timing the close around a tally only you can see,
no bandwagon steering the late voters.

```
voter's browser                       the enclave                        everyone watching
  random token ──sha256──> ballot    {token-hash -> choice}    ──SSE──>  "N votes cast"
  (kept in localStorage)              counts in RAM only                 tally only at close
```

## The trust math

- **Voters have no identity.** A ballot is keyed by the SHA-256 of a random
  token the browser generated; the server never sees a name, cookie, or
  address — the platform's in-TEE proxy is the only thing that ever saw the
  TCP peer. Presenting the same token again *moves* the ballot: one token,
  one vote, revote until close.
- **Sealed means sealed from everyone.** A sealed open poll's JSON simply has
  no `counts` key — not zeros, nothing. The only path to the numbers is
  `POST /api/close` with the admin token, and that path is one-way.
- **The creator is not privileged.** The server stores only the admin token's
  hash; the creator holds the sole close key, but until they use it they
  watch the same participation counter as everyone else.
- **Nothing touches a disk.** The platform gives service apps no filesystem;
  polls are enclave RAM and die with the deployment. Logs carry counts,
  never ids, questions, or choices.

## Features

- Two modes: **sealed** (results at close, the default) and **live**
  (real-time tallies over SSE, for when the bandwagon is the fun part).
- 2–12 options, 280-char questions; 1 / 7 / 30 day expiry.
- **Admin link**: created alongside the share link (`/p/<id>#a=<token>`);
  its hash is the only key that closes the poll and breaks the seal.
- Live participation counter and closed-reveal pushed over SSE; late joiners
  get full state in the stream's first event.
- Live public stats (`/api/stats`) — counts only, by construction.
- Caps: 5 000 polls, 50 000 voters per poll; closed polls stay readable for
  48 h, then the enclave sweeps them; unclosed polls expire tally-unrevealed.

## API

Bodies are percent-encoded lines or `k=v&` forms; JSON is emit-only. The
server never parses JSON, never generates randomness, never picks an id.

| route | in | out |
|---|---|---|
| `POST /api/polls` | headers `x-poll-id`, `x-admin-hash`, `x-mode`, `x-ttl`; body = question + options, one %-encoded value per line | `{ok, expires_at}` |
| `GET /api/poll?id=<id>` | — | state; `counts` present only when live or closed |
| `POST /api/vote` | `id=&choice=&voter=<token>` | `{ok, total_votes}` (+ `counts` in live mode) |
| `POST /api/close` | `id=&admin=<token>` | `{ok, counts}` — idempotent |
| `GET /api/stream?id=<id>` | — | SSE: `tally` / `closed` events |
| `GET /api/stats` | — | counts only |
| `GET /` · `GET /p/<id>` UI · `GET /ping` liveness | | |

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
# → target/wasm32-wasip2/release/ballot.wasm  (~200 KB component)

wasmtime run -Scli -Stcp -Sinherit-network -Sallow-ip-name-lookup \
  --env 'ENCLAVE_PORTS=http:8080=18082' \
  target/wasm32-wasip2/release/ballot.wasm
# then open http://127.0.0.1:18082
```

## Deploy on enclave.host

Publish the component (see the repo README / `guide` topic "publish"), then
deploy CPU-only — the minimum share is plenty:

```
enclave deploy <publisher>/ballot --cpu 0.01 --fund 2
```

Before trusting a vote to a deployment, verify its attestation (guide topic
"attestation"): "sealed until close" is only as credible as the proof of
what's running.
