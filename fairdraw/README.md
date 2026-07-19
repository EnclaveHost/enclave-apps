# fairdraw — provably fair drawings as an Enclave service app

Every raffle, giveaway and prize draw ends the same way: someone claims it
was rigged, and nothing can refute them — whoever held the RNG could have
rolled until they liked the answer. fairdraw makes that accusation
**checkable**. At creation the enclave draws a secret salt and publishes
`sha256(salt)` before a single entry exists; winners are a pure function of
`(salt, the exact entry list)`; at close the salt is revealed and anyone can
replay the draw in their browser and check the commitment. The result page
shows the verification, not just the names.

```
creation                        entries                          close
  salt sealed in enclave RAM      list grows in insertion order    salt revealed
  sha256(salt) published  ──>     each entry = sha256(token)  ──>  winners = f(salt, list)
  (screenshot it)                 one token, one slot              anyone replays f
```

## The trust math

- **The operator can't grind salts.** The commitment is published first;
  by the time anyone could wish for a different outcome, the salt is fixed.
- **Entrants can't grind entries.** The salt never leaves enclave RAM before
  close, so no one can predict which entry list produces which winners.
- **The draw can't be re-rolled.** Winners are a deterministic Fisher-Yates
  prefix seeded by `sha256(salt ‖ sha256(entry hashes, in order))` — there is
  exactly one answer, and the page recomputes it with WebCrypto in front of
  the entrants.
- **Attestation carries the proof.** A hardware-attested TEE, built
  reproducibly from this source via the on-chain catalog, is what makes
  "the code holding the salt is this code" more than a promise.
- **Entrants are anonymous to the server.** An entry is the SHA-256 of a
  random token the browser generated, plus a display name. Re-presenting the
  token renames the slot in place — position stable, so a re-entry can't
  reshuffle the committed order. Logs carry counts, never ids or names.

## Honest about the entropy

The salt is built zero-dep from what std already hands a wasip2 program:
several freshly host-seeded `RandomState` hashers plus the nanosecond clock,
mixed through SHA-256. That is unpredictability, not a hardware RNG claim —
and the fairness argument deliberately doesn't rest on it. Commit-then-reveal
means even a weak salt can't be ground against the entry list (it was fixed
first), and attestation proves nobody could peek at it before close. The
entropy is the floor; the protocol is the proof.

## Features

- 1 / 2 / 3 / 5 / 10 winners, drawn in order (1st, 2nd, …).
- **Manual close** with an admin link, or a **deadline** (1 h – 7 d) the
  enclave enforces itself; both run the same close path.
- Live entrant wall over SSE; the closed page carries a one-click
  **verify panel** that recomputes commitment, entries hash and the full
  Fisher-Yates replay in the browser.
- Closed draws stay up for 7 days so latecomers can verify; an unclosed
  draw's salt is *never* revealed — expiry erases it.
- Caps: 2 000 draws, 10 000 entries per draw, 80-char titles, 40-char names;
  at capacity it says so rather than evicting someone's draw.

## The algorithm (recompute it yourself)

```
entries_hash = sha256(concat of each entry's token-hash hex string, insertion order)
seed         = sha256(salt_bytes ‖ entries_hash_bytes)          # 32 + 32 in
idx          = [0, 1, …, n-1]
for i in 0..k:                                                   # k = min(winners, n)
    r = sha256(seed ‖ i as 8 little-endian bytes)
    j = i + (first 8 bytes of r, little-endian, as u64) % (n - i)
    swap(idx[i], idx[j])
winners = idx[..k], in that order
```

## API

Bodies are `%`-encoded lines or `k=v&` forms; JSON is emit-only. The keys
that decide the result — `salt`, `entry_hashes`, `winners` — exist in
`/api/draw` only once the draw is closed. That asymmetry is the app.

| route | in | out |
|---|---|---|
| `POST /api/draws` | headers `x-draw-id`, `x-admin-hash`, `x-winners`, `x-ttl?`, `x-deadline-in?`; body = `%`-enc title | `{ok, commit, expires_at}` |
| `POST /api/enter` | `id=&name=&token=` | `{ok, count}` — same token renames in place |
| `GET /api/draw?id=` | — | state; `+ winners, salt, entry_hashes` when closed |
| `POST /api/close` | `id=&admin=` | the closed state (idempotent) |
| `GET /api/stream?id=` | — | SSE `entries` / `closed` on topic `<id>` |
| `GET /api/stats` | — | counts only |
| `GET /` , `/d/<id>` UI · `GET /ping` liveness | | |

## Design notes

Zero dependencies: `src/httpd.rs` is the suite's hand-rolled HTTP/1.1 + SSE
engine (one non-blocking event loop, the nanircd shape — wasip2 has no
threads), `src/sha256.rs` is FIPS 180-4 by hand, and the whole UI is one
embedded HTML file whose verify panel is plain WebCrypto. The one platform
rule, as ever: **read `ENCLAVE_PORTS`, bind the actual port** — the
deployment's `http:` entry is served at its origin by the enclave's in-TEE
TLS proxy.

## Build & test

```bash
rustup target add wasm32-wasip2        # or your distro's wasip2 std
cargo build --release --target wasm32-wasip2
# → target/wasm32-wasip2/release/fairdraw.wasm  (~210 KB component)

wasmtime run -Scli -Stcp -Sinherit-network -Sallow-ip-name-lookup \
  --env 'ENCLAVE_PORTS=http:8080=18089' \
  target/wasm32-wasip2/release/fairdraw.wasm
# then open http://127.0.0.1:18089
```

## Deploy on enclave.host

Publish the component (see the repo README / `guide` topic "publish"), then
deploy CPU-only — the minimum share is plenty:

```
enclave deploy <publisher>/fairdraw --cpu 0.01 --fund 2
```

Before running a draw people care about, verify the deployment's attestation
(guide topic "attestation"): the commitment only proves what the *code*
did — attestation proves which code.
