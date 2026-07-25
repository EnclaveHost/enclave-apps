# tripwire — canary tokens with a live alarm board, as an Enclave service app

Plant a URL somewhere nothing should ever touch it — a `passwords.xlsx` on a
file share, a honeypot row in a database dump, a fake AWS key committed to a
repo, a 1×1 pixel in a document. Nobody has a legitimate reason to fetch it.
So a single fetch isn't a signal to correlate, it's a **fact**: something read
a thing it shouldn't have, at this second, with this user-agent, from this
referer. Canary tokens are the cheapest intrusion detection there is — no
agents, no baselines, no tuning, no false positives.

The part a VPS can't offer: the trip log is **append-only inside an attested
TEE**. An intruder who owns the host still cannot delete the record of their
own trip.

```
you, once                          the enclave                    you, the moment it fires
  plant /t/<token>   ──fetch──>   {board -> wires,      ──SSE──>   the board goes red
  in a honeypot file               trips} in RAM                   ⚠ 1 alarm
  (an intruder, later)             append-only                     what · when
```

## The trust math

- **The evidence claim, stated honestly.** Trips are recorded by attested code
  holding RAM-only state, into a per-wire ring nothing in the API can rewind,
  edit, or edit *around*. Whoever owns the machine cannot reach in and quietly
  unfire an alarm. They **can** stop the deployment — but stopping it is
  itself visible, and a stopped board is not a clean board. What they cannot
  do is rewrite history and leave it running as if nothing happened.
- **What and when, never who.** The enclave's TLS proxy terminates the
  connection, so no client address ever reaches this process. A trip records
  method, target, user-agent, referer and timestamp — and that is the whole
  list. tripwire tells you that you were breached and when; it does not tell
  you by whom, and it won't imply otherwise. (Nor is the useful direction
  attribution: knowing the hour is what lets you go read the logs that *do*
  have addresses, before they roll.)
- **Every canary URL answers the same nothing.** Live wire, disarmed wire,
  token that never existed — all return the identical dull 404. An intruder
  who harvests tokens and probes them learns nothing about which were armed,
  and gets no hint that an alarm just fired. The alarm is silent by
  construction, not by politeness.
- **The board has no public view.** Publishing which of your wires are quiet
  tells an intruder exactly where to step. Every read — the board JSON and the
  live stream alike — is gated on the owner token, checked against its
  SHA-256. There is no unauthenticated shape of this app but the decoy.
- **Nothing touches a disk.** State is enclave RAM and dies with the
  deployment: tamper-evident while it lives, gone when it dies. Logs carry
  counts — never a board id, a canary token, or anything a trip recorded.

## What the honesty costs, spelled out

A tripwire board is a *witness*, not an archive. If the deployment stops, the
log stops with it — the guarantee is that history can't be silently rewritten,
not that it survives the machine. If that trade is wrong for you, the right
move is to treat a trip as a page, not a filing cabinet: the alarm reaches
your browser the second it fires.

## Features

- Three kinds of wire, differing only in the decoy body served back:
  **link** (a URL you paste into a doc or config), **pixel** (a 43-byte 1×1
  transparent GIF for email/HTML — the fetch fires the alarm long before any
  renderer cares about the status line), **doc** (a fake file link).
- Any method trips: a scanner's `HEAD`, a mail client's `GET`, a script's
  `POST`. The full original target, query string included, is evidence.
- Live board over SSE: a `trip` event flashes the wire card red the instant a
  fetch lands, bumps its count, and sets the tab title to `⚠ N alarms` until
  you acknowledge it.
- Owner link only (`/b/<id>#o=<token>`) — the token rides the URL fragment and
  is only ever *sent* to prove ownership.
- Caps: 500 boards, 25 wires each, a 100-trip ring per wire (the board says
  plainly how many older trips have aged out rather than implying the log is
  complete), 20 trips per wire in the JSON; a board dies after 30 days without
  a trip, an edit, or an owner visit.

## API

Bodies are `k=v&` forms; JSON is emit-only. Board ids, owner tokens and canary
tokens are all client-generated — the server never picks a name and never
generates randomness.

| route | in | out |
|---|---|---|
| `POST /api/boards` | headers `x-board-id`, `x-owner-hash` | `{ok}` |
| `POST /api/wires` | `id=&owner=&name=&kind=&token=` | `{ok}` |
| `POST /api/wires/remove` | `id=&owner=&token=` | `{ok}` |
| `ANY /t/<token>` | the trip — any method, query kept | an innocuous 404 decoy |
| `GET /api/board?id=&owner=` | — | wires + newest-first trips; **owner only** |
| `GET /api/stream?id=&owner=` | — | SSE `trip` on topic `<id>`; **owner only** |
| `GET /api/stats` | — | counts only |
| `GET /`, `/b/<id>` UI · `GET /ping` liveness | | |

Wrong owner is `403 {"error":"denied"}` on every gated route; an unknown board
is the same `404` as an expired one.

## Design notes

Zero dependencies: `src/httpd.rs` is the suite's hand-rolled HTTP/1.1 + SSE
engine (one non-blocking event loop, the nanircd shape — wasip2 has no
threads), `src/sha256.rs` is FIPS 180-4 by hand, and the whole UI is one
embedded HTML file. The trip is the hot path and the only path an attacker
touches, so `canary token → board` is one hash lookup kept in sync on
add/remove/sweep, and the response it builds is byte-identical to the one an
unknown token gets. The one platform rule, as ever: **read `ENCLAVE_PORTS`,
bind the actual port** — the deployment's `http:` entry is served at its origin
by the enclave's in-TEE TLS proxy.

## Build & test

```bash
rustup target add wasm32-wasip2        # or your distro's wasip2 std
cargo build --release --target wasm32-wasip2
# → target/wasm32-wasip2/release/tripwire.wasm  (~213 KB component)

wasmtime run -Scli -Stcp -Sinherit-network -Sallow-ip-name-lookup \
  --env 'ENCLAVE_PORTS=http:8080=18091' \
  target/wasm32-wasip2/release/tripwire.wasm
# then open http://127.0.0.1:18091
```

## Deploy on enclave.host

Publish the component (see the repo README / `guide` topic "publish"), then
deploy CPU-only — the minimum share is plenty:

```
enclave deploy <publisher>/tripwire --cpu 0.01 --fund 2
```

Then plant what the board hands you, and don't touch it again:

```
# in a file called backup-keys.txt on the share nobody should browse
aws_secret_access_key = see https://<origin>/t/<token>

# or as a pixel, in a document you want to know has been opened
<img src="https://<origin>/t/<token>" width="1" height="1">
```

Before trusting a deployment as your witness, verify its attestation (guide
topic "attestation") — "an alarm the intruder can't erase" is only worth
saying because you don't have to take anyone's word for what's running,
including ours.
