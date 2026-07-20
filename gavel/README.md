# gavel — sealed-bid auctions as an Enclave service app

The Vickrey (second-price) auction is the mechanism economists actually want:
when the winner pays the *second*-highest bid, every bidder's dominant
strategy is to bid exactly what the item is worth to them. It is almost never
used online, because it demands **absolute** trust in the auctioneer —
whoever can see the sealed bids can insert a shill bid one unit under the
winner and pocket the difference, and nobody can ever prove it happened.
gavel makes that move *unconstructible*: bids sit sealed in attested enclave
RAM where nobody — the seller included — can read them; the hammer is a pure
function of the bid set; and the only things that ever leave the enclave are
the winner's name and the clearing price.

```
creation                      sealed bids                        the hammer
  reserve blinded+committed     amounts in enclave RAM only        winner + clearing price out
  sha256(salt‖reserve) out ──>  seller's admin link sees ──>       reserve + salt revealed
  hammer time fixed             nothing until the hammer           losing bids SCRUBBED, not shown
```

## The trust math

- **The seller can't shill.** A shill bid needs to know the current high bid;
  the enclave tells no one, admin token included — `/api/result` answers 409
  until the hammer falls. Bidding blind risks winning your own item; the
  honest tool for a floor is the reserve, committed before bidding starts.
- **The reserve can't chase the bids.** `sha256(salt ‖ reserve)` is published
  at creation — before the first bid exists — and opens at close. It's
  committed even when there is *no* reserve, so bidders can't tell.
- **Losing bids are never revealed — to anyone, ever.** At the hammer the
  enclave computes the result, publishes per-bid fingerprints, then scrubs
  every losing amount, name and contact from RAM. In second-price mode even
  the *winner's own amount* is scrubbed; they pay the runner-up's bid.
- **Every bidder can check they were counted.** The close publishes
  `sha256(token_hash ":" amount)` per bid; only the holder of the token can
  recompute theirs. Inclusion proves the hammer weighed your bid;
  the count proves nothing was added.
- **Attestation carries the rest.** Correct selection of the max is exactly
  what "the code holding the bids is this code" buys — a hardware-attested
  TEE, built reproducibly from this source via the on-chain catalog.
- **Bidders are anonymous to the server.** A bid is keyed by the SHA-256 of a
  random browser-held token. Re-presenting the token re-bids the same slot —
  priority stable, ties go to the earlier slot, so raising your bid keeps
  your place in line. Logs carry counts, never ids, names or amounts.

## Modes and settlement

| | winner | pays | revealed at close |
|---|---|---|---|
| **second** (Vickrey, default) | highest bid ≥ reserve | `max(second-highest, reserve, 1)` | price only — winner's bid stays sealed forever |
| **first** (sealed tender) | highest bid ≥ reserve | their own bid | the price *is* the winning bid |

Ties go to the earlier slot. No bid at or above the reserve → **no sale**;
the reserve still opens, proving it was fixed all along. Single bid, no
reserve, second-price → the clearing price floors at 1 unit: set a reserve if
you care. Amounts are whole numbers (up to 13 digits) in whatever unit the
seller labels — USD, sats, points.

A **cancel** (admin link, while open) erases every sealed bid on the spot and
reveals nothing, reserve included.

## Features

- Second-price or first-price; hammer deadline 1 h – 7 d (UI; API to 30 d),
  enforced by the enclave — or dropped early by the admin link (which is
  harmless precisely because it can't see the bids).
- Optional blinded reserve and free-form unit label; 500-char item notes.
- Winner's optional contact line, shown only to the seller, only at close;
  losers' contacts are scrubbed unread.
- Live bid *count* over SSE; the closed page carries a one-click **verify
  panel** — commitment opening and your own fingerprint inclusion, plain
  WebCrypto in the browser.
- `/api/mine` echo: while open, proof your seal took; after, `won` with what
  you owe or a uniform `lost` — no rank, no distance.
- Closed auctions stay checkable for 7 days, cancelled ones for 1.
- Caps: 2 000 auctions, 10 000 bids per auction, 80-char titles, 40-char
  names; at capacity it says so rather than evicting someone's auction.

## API

Bodies are `k=v&` forms, `%`-encoded; JSON is emit-only. The keys that decide
the result — `reserve`, `salt`, `fingerprints`, `winner`, `price` — exist in
`/api/auction` only once the hammer has fallen. That asymmetry is the app.

| route | in | out |
|---|---|---|
| `POST /api/auctions` | headers `x-auction-id`, `x-admin-hash`, `x-mode` (`second`\|`first`), `x-deadline-in`, `x-reserve?`, `x-unit?`; body `title=&desc=` | `{ok, commit, closes_at}` |
| `POST /api/bid` | `id=&token=&amount=&name=&contact=?` | `{ok, count}` — same token re-bids in place |
| `GET /api/auction?id=` | — | state; `+ sold, price, winner, reserve, salt, fingerprints` when closed |
| `POST /api/close` | `id=&admin=` | the closed state (idempotent early hammer) |
| `POST /api/cancel` | `id=&admin=` | bids erased, nothing revealed |
| `GET /api/mine?id=&token=` | — | your echo while open; `won/pay` or uniform `lost` after |
| `GET /api/result?id=&admin=` | — | seller's view — 409 until closed |
| `GET /api/stream?id=` | — | SSE `bids` / `closed` / `cancelled` on topic `<id>` |
| `GET /api/stats` | — | counts only |
| `GET /` , `/a/<id>` UI · `GET /ping` liveness | | |

## Design notes

Zero dependencies: `src/httpd.rs` is the suite's hand-rolled HTTP/1.1 + SSE
engine (one non-blocking event loop, the nanircd shape — wasip2 has no
threads), `src/sha256.rs` is FIPS 180-4 by hand, and the whole UI is one
embedded HTML file whose verify panel is plain WebCrypto. The one platform
rule, as ever: **read `ENCLAVE_PORTS`, bind the actual port** — the
deployment's `http:` entry is served at its origin by the enclave's in-TEE
TLS proxy.

The commitment salt is built zero-dep from what std hands a wasip2 program
(host-seeded `RandomState` hashers + the nanosecond clock through SHA-256);
it only *blinds* the reserve, and the auction's integrity doesn't rest on it
— see fairdraw's "honest about the entropy" note for the framing.

## Build & test

```bash
rustup target add wasm32-wasip2        # or your distro's wasip2 std
cargo build --release --target wasm32-wasip2
# → target/wasm32-wasip2/release/gavel.wasm  (~230 KB component)

wasmtime run -Scli -Stcp -Sinherit-network -Sallow-ip-name-lookup \
  --env 'ENCLAVE_PORTS=http:8080=18080' \
  target/wasm32-wasip2/release/gavel.wasm
# then open http://127.0.0.1:18080
```

## Deploy on enclave.host

Publish the component (see the repo README / `guide` topic "publish"), then
deploy CPU-only — the minimum share is plenty:

```
enclave deploy <publisher>/gavel --cpu 0.01 --fund 2
```

Before auctioning anything people care about, verify the deployment's
attestation (guide topic "attestation"): the reveal only proves the numbers
are consistent — attestation proves no one could peek at the bids.
