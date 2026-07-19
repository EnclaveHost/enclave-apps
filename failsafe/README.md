# failsafe — a dead-man's switch and time capsule for secrets, as an Enclave service app

A one-page service for postdating a secret: a **time capsule** releases the
ciphertext at a fixed future moment, a **dead-man's switch** releases it only
if the owner stops checking in. Any server can promise "nobody reads this
early, and I'll publish it if you go silent" — but its operator can also read
the disk, serve the blob to a friend, or quietly never release it. Here the
gate is code running inside a **hardware-attested TEE**, reproducible from
this source via the on-chain catalog: until the moment arrives, the only copy
of the ciphertext sits in enclave RAM behind a process that provably refuses
to serve it — to the recipients holding the link, to the platform operator,
and to the sender themselves.

```
sender's browser                     the enclave                     recipient's browser
  AES-256-GCM encrypt   ──POST──>   {id -> blob, release_at,    ──POST /take──>  decrypt
  key -> link #fragment              reads, owner hash} in RAM        (423 until release;
  owner token -> own link            check-in pushes release_at       key from #fragment)
```

## The trust math

- **The key never travels.** It rides the recipients' link URL *fragment*
  (`https://…/#<id>.<key>`), which browsers do not send in requests. The
  server time-gates ciphertext it cannot read, under an id the *client* chose.
- **Sealed means sealed.** A take before the release moment is a `423 Locked`
  with a countdown — the blob is not in the response, so there is nothing to
  leak. The transition is one comparison in one single-threaded process.
- **The switch credential is a hash.** Check-in and disarm present a token
  the server only ever stored the SHA-256 of; every check-in pushes the
  deadline out by the interval, disarm erases in any state.
- **Misses are uniform.** Unknown, disarmed, read-out and lapsed ids are the
  same 404 — holding a dead link proves nothing about what existed.
- **Nothing touches a disk.** The platform gives service apps no filesystem;
  state is enclave RAM and dies with the deployment. Logs carry counts, never
  ids or blobs.
- **The honest caveat: release timing trusts the platform clock.** The
  enclave asks the host what time it is — the same trust per-second billing
  already requires. Attestation removes the operator's ability to *read* the
  secret early or fake a check-in; a hostile host could still skew when
  "later" arrives.

## Features

- Two modes per drop: **capsule** (releases in 1 hour – 30 days, immovable)
  or **switch** (check-in interval 24 hours – 30 days, rolling deadline).
- 1 / 3 / 10 / 25 reveals once released; released ciphertext stays readable
  for a 7-day window, then the only copy is erased — read or not.
- **Owner link** (`#!<id>.<token>`): required at arm time. It is both the
  check-in and the disarm credential; losing it means nobody can stop the
  release.
- Recipients see a live countdown before release — proof the drop exists,
  nothing more.
- Live public stats (`/api/stats`) — counts only, by construction.
- Caps: 64 KiB plaintext, 20 000 drops, 48 MiB total; at capacity it says so
  rather than evicting someone's secret.

## API

Bodies are opaque base64url blobs or `k=v&` forms; JSON is emit-only. The
server never parses JSON, never generates randomness, never picks an id.

| route | in | out |
|---|---|---|
| `POST /api/arm` | headers `x-drop-id`, `x-mode`, `x-release-in`\|`x-interval`, `x-window?`, `x-reads?`, `x-owner-hash`; body = blob | `{ok, release_at}` |
| `POST /api/peek` | `id=<id>` | `{mode, released, releases_in, reads_left, size, window_left}` |
| `POST /api/checkin` | `id=<id>&token=<secret>` | `{ok, releases_in}` — switch only, pre-release only |
| `POST /api/disarm` | `id=<id>&token=<secret>` | `{ok}` — erases in any state |
| `POST /api/take` | `id=<id>` | `423 {sealed, releases_in}` until release; then `{blob, reads_left}` — decrements, erases at 0 |
| `GET /api/stats` | — | counts only |
| `GET /` UI · `GET /ping` liveness | | |

## Design notes

Zero dependencies: `src/httpd.rs` is the suite's hand-rolled HTTP/1.1 + SSE
engine (one non-blocking event loop, the nanircd shape — wasip2 has no
threads), `src/sha256.rs` is FIPS 180-4 by hand, and the whole UI is one
embedded HTML file with inline WebCrypto. No SSE here — the countdown ticks
client-side and re-peeks each minute to stay honest about check-ins. The one
platform rule, as ever: **read `ENCLAVE_PORTS`, bind the actual port** — the
deployment's `http:` entry is served at its origin by the enclave's in-TEE
TLS proxy.

## Build & test

```bash
rustup target add wasm32-wasip2        # or your distro's wasip2 std
cargo build --release --target wasm32-wasip2
# → target/wasm32-wasip2/release/failsafe.wasm  (~190 KB component)

wasmtime run -Scli -Stcp -Sinherit-network -Sallow-ip-name-lookup \
  --env 'ENCLAVE_PORTS=http:8080=18080' \
  target/wasm32-wasip2/release/failsafe.wasm
# then open http://127.0.0.1:18080
```

## Deploy on enclave.host

Publish the component (see the repo README / `guide` topic "publish"), then
deploy CPU-only — the minimum share is plenty:

```
enclave deploy <publisher>/failsafe --cpu 0.01 --fund 2
```

Before arming anything that matters, verify the deployment's attestation
(guide topic "attestation"): a dead-man's switch is only as credible as your
proof of what's holding it.
