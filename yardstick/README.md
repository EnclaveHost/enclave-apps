# yardstick — aggregate-only group measurement as an Enclave service app

"What's your salary?" is the question everyone wants answered and nobody
wants to answer first. Same for day rates, rents, seed valuations, sprint
estimates. Every existing way to pool those numbers routes them through
*someone* — a spreadsheet owner, a survey vendor, that one trusted colleague
— who then **knows**. yardstick replaces the someone with attested enclave
RAM: each person submits one integer; at close the enclave reveals only
aggregate statistics, and only if at least *k* people submitted; the
individual numbers are scrubbed in the same pass. Below quorum, nothing is
revealed and everything is scrubbed anyway — a threshold enforced by code
anyone can attest, not by a promise.

```
creation                      sealed numbers                     close
  quorum k fixed (floor 3)      one integer per token, in RAM      count >= k: median/mean/quartiles out
  organizer holds no key   ──>  re-submit replaces your own   ──>  count <  k: NOTHING out
                                organizer sees count only          either way: every number scrubbed
```

## The trust math

- **The organizer never sees a number.** The admin link can close the
  measurement; it cannot read a submission — there is no endpoint that
  returns one, at any time, to anyone.
- **The quorum is code, not policy.** A close below *k* reveals no median,
  no mean — not even of what was there. The submitted numbers are scrubbed
  unread.
- **Reveal is also erasure.** The instant aggregates are computed, every
  individual value is zeroed in RAM. There is nothing left to leak,
  subpoena, or breach — the aggregates are the only thing that ever existed
  outside the enclave.
- **Every participant can check they were counted.** The reveal publishes
  per-submission fingerprints `sha256(token_hash ":" value)`; only the token
  holder can recompute theirs. Inclusion + the published count bound the set
  the stats were computed from.
- **Submitters are anonymous to the server.** A submission is keyed by the
  SHA-256 of a browser-held random token; re-presenting it replaces your own
  number in place. Logs carry counts, never ids or values.
- **Honest about what aggregates say.** The median of three numbers *is* the
  middle person's number — it just doesn't say whose. Any quantile is
  someone's value. That's inherent to aggregation; pick *k* to taste (the UI
  defaults to 5) and read small-n stats accordingly. And as with any survey,
  the organizer can submit a skewing number — what they can't do here is
  *see* yours.

## What's revealed (exact integer math)

| statistic | when | method |
|---|---|---|
| `count` | always (it was public all along) | |
| `median` | count ≥ k | middle value; even count → mean of the two middles (`x.5` exact) |
| `mean` | count ≥ k | one decimal, round-half-up, exact integer arithmetic |
| `p25` / `p75` | count ≥ 5 | nearest-rank, 1-based `ceil(q·n)` |

Never revealed: min, max, any individual value, or anything at all below
quorum. Values are whole numbers 0 – 9 999 999 999 999 in whatever unit the
organizer labels.

## Features

- Quorum 3 – 100 (UI: 3/5/8/10/20, default 5); manual close via admin link
  or a deadline (1 h – 7 d) the enclave enforces itself — same close path.
- Live participation meter over SSE (count only, of course).
- `/api/mine` echo: while open, proof your seal took (and what you'd change
  it to); after close, only *whether* you were counted — the value no longer
  exists to be echoed.
- One-click verify panel at reveal: fingerprint count + your own inclusion,
  plain WebCrypto in the browser.
- Revealed stats stay up 7 days; an unclosed measurement expires (default
  7 d, max 30 d) with its numbers unread.
- Caps: 2 000 measurements, 10 000 submissions each, 80-char titles; at
  capacity it says so rather than evicting someone's measurement.

## API

Bodies are `k=v&` forms, `%`-encoded; JSON is emit-only. The keys that carry
information — `median`, `mean`, `p25`, `p75`, `fingerprints` — exist in
`/api/measure` only once the measurement closed *at quorum*. That asymmetry
is the app.

| route | in | out |
|---|---|---|
| `POST /api/measures` | headers `x-measure-id`, `x-admin-hash`, `x-quorum`, `x-unit?`, `x-ttl?`, `x-deadline-in?`; body `title=&desc=` | `{ok, quorum, expires_at}` |
| `POST /api/submit` | `id=&token=&value=` | `{ok, count}` — same token replaces in place |
| `GET /api/measure?id=` | — | state; `+ revealed, median, mean, p25, p75, fingerprints` when closed at quorum |
| `POST /api/close` | `id=&admin=` | the closed state (idempotent) |
| `GET /api/mine?id=&token=` | — | your echo while open; `counted` after |
| `GET /api/stream?id=` | — | SSE `subs` / `closed` on topic `<id>` |
| `GET /api/stats` | — | counts only |
| `GET /` , `/m/<id>` UI · `GET /ping` liveness | | |

## Design notes

Zero dependencies: `src/httpd.rs` is the suite's hand-rolled HTTP/1.1 + SSE
engine (one non-blocking event loop, the nanircd shape — wasip2 has no
threads), `src/sha256.rs` is FIPS 180-4 by hand, and the whole UI is one
embedded HTML file whose verify panel is plain WebCrypto. The one platform
rule, as ever: **read `ENCLAVE_PORTS`, bind the actual port** — the
deployment's `http:` entry is served at its origin by the enclave's in-TEE
TLS proxy.

No differential-privacy noise, deliberately: v1 keeps the math exact and the
claims honest — the protections are the quorum, the scrub, and attestation,
and the README says exactly what a quantile still tells you.

## Build & test

```bash
rustup target add wasm32-wasip2        # or your distro's wasip2 std
cargo build --release --target wasm32-wasip2
# → target/wasm32-wasip2/release/yardstick.wasm  (~230 KB component)

wasmtime run -Scli -Stcp -Sinherit-network -Sallow-ip-name-lookup \
  --env 'ENCLAVE_PORTS=http:8080=18081' \
  target/wasm32-wasip2/release/yardstick.wasm
# then open http://127.0.0.1:18081
```

## Deploy on enclave.host

Publish the component (see the repo README / `guide` topic "publish"), then
deploy CPU-only — the minimum share is plenty:

```
enclave deploy <publisher>/yardstick --cpu 0.01 --fund 2
```

Before measuring anything people care about, verify the deployment's
attestation (guide topic "attestation"): the fingerprints only prove your
number was counted — attestation proves nobody could read it.
