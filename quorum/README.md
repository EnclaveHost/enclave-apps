# quorum — M-of-N approval release for secrets, as an Enclave service app

A one-page service for the two-person rule: seal a secret so that **M of N
designated approvers** must present their tokens before anyone — including
the person holding the recipient link — can read it. The classic use cases
are break-glass escrow (the on-call root credential that needs two seniors
to agree it's an emergency) and dual-control release of anything too
dangerous for one set of hands. The rule only works if no single person can
conjure the missing signature — and on an ordinary server, one person always
can: whoever operates it can read the disk, flip the released flag, or forge
the approval ledger. Here the code that counts approvals runs inside a
**hardware-attested TEE**, reproducible from this source via the on-chain
catalog: the operator can't skip the quorum any more than a stranger can,
and "M of N agreed" stops being a promise and becomes a property.

```
creator's browser                      the enclave                    recipient's browser
  AES-256-GCM encrypt    ──POST──>   {id -> blob, M, N hashes,   ──POST /take──>  decrypt
  key -> recipient #fragment          approval bits} in RAM           (423 until quorum;
  N tokens -> approver links          approve flips one bit            key from #fragment)
```

## The trust math

- **The key never travels.** It rides the recipient's link URL *fragment*
  (`https://…/#<id>.<key>`), which browsers do not send in requests. The
  server quorum-gates ciphertext it cannot read, under an id the *client*
  chose.
- **Sealed means sealed.** A take before the quorum is a `423 Locked` with a
  count — the blob is not in the response, so there is nothing to leak. The
  count-and-compare is one step in one single-threaded process.
- **Approvals are hashes, and private.** Each approver holds a token the
  server only ever stored the SHA-256 of; presenting it flips that seat's
  bit — idempotently, so nobody counts twice. **Which seats approved is
  never disclosed**: peek, take, and even the approve response carry only
  the count. An approver can withdraw before the quorum is met; once M is
  reached the release is one-way and approvals are final.
- **Unmet quorums vanish.** If M approvals don't land by the armed deadline,
  the sweep erases the only copy unrevealed. Once met, the blob stays
  readable for a bounded window, then goes the same way.
- **Misses are uniform.** Unknown, expired-unmet, burned and read-out ids
  are the same 404 — holding a dead link proves nothing about what existed.
- **Nothing touches a disk.** The platform gives service apps no filesystem;
  state is enclave RAM and dies with the deployment. Logs carry counts,
  never ids or blobs.

## Features

- 1–12 approvers, any threshold M ≤ N (the UI defaults to a strict
  majority); quorum deadline 24 hours / 7 days / 30 days.
- 1 / 3 / 10 reveals once released; released ciphertext stays readable for a
  7-day window, then the only copy is erased — read or not.
- **Approver links** (`#@<id>.<token>`), one per seat — distribute them over
  separate channels; a channel carrying two carries two signatures.
- **Creator burn link** (`#!<id>.<token>`): revokes in any state, sealed or
  released — erasure is the one thing no quorum gates.
- Live public stats (`/api/stats`) — counts only, by construction.
- Caps: 64 KiB plaintext, 20 000 drops, 48 MiB total; at capacity it says so
  rather than evicting someone's secret.

## API

Bodies are opaque base64url blobs or `k=v&` forms; JSON is emit-only. The
server never parses JSON, never generates randomness, never picks an id.

| route | in | out |
|---|---|---|
| `POST /api/arm` | headers `x-drop-id`, `x-threshold`, `x-approver-hashes` (comma-joined sha256 hexes), `x-reads?`, `x-ttl?`, `x-window?`, `x-burn-hash?`; body = blob | `{ok, expires_at}` |
| `POST /api/peek` | `id=<id>` | `{threshold, approvers, approvals, released, reads_left, size, expires_in, window_left}` — the count, never the ledger |
| `POST /api/approve` | `id=<id>&token=<secret>` | `{ok, approvals, threshold, released}` — idempotent per seat |
| `POST /api/revoke-approval` | `id=<id>&token=<secret>` | `{ok, approvals}` pre-release; `409 released` after — a met quorum is final |
| `POST /api/take` | `id=<id>` | `423 {sealed, approvals, threshold}` until quorum; then `{blob, reads_left}` — decrements, erases at 0 |
| `POST /api/burn` | `id=<id>&token=<secret>` | `{ok}` if SHA-256 matches — any state |
| `GET /api/stats` | — | counts only |
| `GET /` UI · `GET /ping` liveness | | |

## Design notes

Zero dependencies: `src/httpd.rs` is the suite's hand-rolled HTTP/1.1 + SSE
engine (one non-blocking event loop, the nanircd shape — wasip2 has no
threads), `src/sha256.rs` is FIPS 180-4 by hand, and the whole UI is one
embedded HTML file with inline WebCrypto. No SSE here — approvals are rare
events, so the sealed views just re-peek every 20 seconds to stay honest
about the count. The one platform rule, as ever: **read `ENCLAVE_PORTS`,
bind the actual port** — the deployment's `http:` entry is served at its
origin by the enclave's in-TEE TLS proxy.

## Build & test

```bash
rustup target add wasm32-wasip2        # or your distro's wasip2 std
cargo build --release --target wasm32-wasip2
# → target/wasm32-wasip2/release/quorum.wasm  (~200 KB component)

wasmtime run -Scli -Stcp -Sinherit-network -Sallow-ip-name-lookup \
  --env 'ENCLAVE_PORTS=http:8080=18080' \
  target/wasm32-wasip2/release/quorum.wasm
# then open http://127.0.0.1:18080
```

## Deploy on enclave.host

Publish the component (see the repo README / `guide` topic "publish"), then
deploy CPU-only — the minimum share is plenty:

```
enclave deploy <publisher>/quorum --cpu 0.01 --fund 2
```

Before trusting a deployment with a real break-glass secret, verify its
attestation (guide topic "attestation"): a two-person rule is only as strong
as your proof of what's counting the signatures.
