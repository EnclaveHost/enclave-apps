# pulse — push-based heartbeat monitoring as an Enclave service app

An uptime monitor in the healthchecks.io family, inverted to fit an enclave
exactly: nothing here probes your machines, because a no-egress enclave
can't — and doesn't need to. Your cron jobs, backup scripts and services
curl a per-monitor heartbeat URL **into** the enclave on their own schedule,
and the live status page turns red when the beats stop. No agents, no
inbound firewall holes, no probe traffic: if a job can run `curl`, it's
monitored.

```
your cron job                        the enclave                   anyone's browser
  … && curl -fsS    ──/hb/<tok>──>   {page -> monitors,    ──SSE──>  live status page
  (on its own schedule)               beat arrivals} in RAM           waiting/up/late/down
```

## The trust math

- **The history claim, stated honestly.** Statuses derive from beat arrival
  times recorded by attested code holding RAM-only state. Nobody — including
  whoever hosts the page — can backfill a missed beat, edit history, or
  quietly drop an embarrassing gap: what's green was actually beating. (What
  no TEE can prove: that your job was *healthy* — only that it called in.
  Timing trusts the platform clock, the same trust billing already requires.)
- **Beat URLs are capabilities.** Client-generated, one monitor each,
  forever; whoever holds one can feed that monitor and nobody else can. They
  never appear on the public page, in the stream, in logs, or in stats — the
  owner token (checked against its SHA-256) is the only way to read one back.
- **Status is pure derivation.** never beat → `waiting`; within the period →
  `up`; inside the grace → `late`; past it → `down`. One shared function
  decides, for the JSON, the stream, and the change tick alike.
- **Misses are uniform.** Unknown pages and unknown beat tokens are the same
  404 — holding a dead URL proves nothing about what existed.
- **Nothing touches a disk.** State is enclave RAM and dies with the
  deployment: tamper-evident while it lives, gone when it dies. Logs carry
  counts, never ids, names, or tokens.

## Features

- Up to 20 monitors per page; periods 1 m – 24 h with 0 – 24 h grace.
- Live page: SSE `beat` events flash the dot the moment a curl lands; a 15 s
  change tick pushes a full refresh when anything slides to late/down.
- A 12-segment regularity strip per monitor — the last inter-beat gaps,
  on-time green, in-grace amber, late red.
- Public link (`/s/<id>`) and owner link (`/s/<id>#o=<token>`); the owner
  token rides the URL fragment and is only ever *sent* to prove ownership.
- Caps: 500 pages, 20 monitors each, 50 beat timestamps per monitor; a page
  dies after 30 days without a beat, an edit, or a viewer.

## API

Bodies are `k=v&` forms; JSON is emit-only. Page ids, owner tokens and beat
tokens are all client-generated — the server never picks a name.

| route | in | out |
|---|---|---|
| `POST /api/pages` | headers `x-page-id`, `x-owner-hash` | `{ok}` |
| `POST /api/monitors` | `id=&owner=&name=&period=&grace=&beat=` | `{ok}` |
| `POST /api/monitors/remove` | `id=&owner=&beat=` | `{ok}` |
| `GET`/`POST /hb/<beat-token>` | the heartbeat — query/body ignored | `{ok, next_due}` |
| `GET /api/page?id=<id>[&owner=]` | — | statuses; owner sees `beat_token`s |
| `GET /api/stream?id=<id>` | — | SSE `beat` / `page` on topic `<id>` |
| `GET /api/stats` | — | counts only |
| `GET /`, `/s/<id>` UI · `GET /ping` liveness | | |

## Design notes

Zero dependencies: `src/httpd.rs` is the suite's hand-rolled HTTP/1.1 + SSE
engine (one non-blocking event loop, the nanircd shape — wasip2 has no
threads), `src/sha256.rs` is FIPS 180-4 by hand, and the whole UI is one
embedded HTML file. The heartbeat is the hot path, so `beat token → page` is
one hash lookup kept in sync on add/remove/sweep. The one platform rule, as
ever: **read `ENCLAVE_PORTS`, bind the actual port** — the deployment's
`http:` entry is served at its origin by the enclave's in-TEE TLS proxy.

## Build & test

```bash
rustup target add wasm32-wasip2        # or your distro's wasip2 std
cargo build --release --target wasm32-wasip2
# → target/wasm32-wasip2/release/pulse.wasm  (~210 KB component)

wasmtime run -Scli -Stcp -Sinherit-network -Sallow-ip-name-lookup \
  --env 'ENCLAVE_PORTS=http:8080=18088' \
  target/wasm32-wasip2/release/pulse.wasm
# then open http://127.0.0.1:18088
```

## Deploy on enclave.host

Publish the component (see the repo README / `guide` topic "publish"), then
deploy CPU-only — the minimum share is plenty:

```
enclave deploy <publisher>/pulse --cpu 0.01 --fund 2
```

Then end a cron line with the beat URL the page hands you:

```
0 3 * * *  /usr/local/bin/backup.sh && curl -fsS https://<origin>/hb/<token>
```

Before pointing dashboards at a deployment, verify its attestation (guide
topic "attestation") — "history nobody can edit" is only worth saying
because you don't have to take anyone's word for it.
