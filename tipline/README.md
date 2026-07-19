# tipline — an anonymous encrypted inbox as an Enclave service app

A SecureDrop-lite tip line: publish one link, and anyone can send you a message
that only you can read. Tips are encrypted in the sender's browser to a public
key you hold; the server stores ciphertext it can never open. The part ordinary
web apps can't offer — and the reason this belongs in an enclave — is that the
**submission page itself is attested**, reproducible from this source via the
on-chain catalog. A source can verify the code before typing a word.

```
sender's browser                      the enclave                    your browser
  ECDH(ephemeral, your pub)  ──POST──>  {line -> [sealed tips]}   ──GET /inbox──>  decrypt
  → AES-256-GCM ciphertext             ring in RAM, gated on          private key from
  ephemeral key discarded              the owner-token hash           the owner link #fragment
```

## Why this needs a TEE (the whole point)

The crypto here is ordinary public-key crypto. A tip line is **asymmetric**:
many senders, one recipient. You publish a *public* key; senders encrypt to it;
only your *private* key opens anything. A server holding ciphertext it can't
read is nothing new — that's just encryption.

What is *not* ordinary is this: **a source has to trust the page they type
into.** The encryption happens in JavaScript that some server hands the sender.
Any ordinary host can quietly serve a modified submission page — one that
exfiltrates the plaintext before encrypting it, or swaps in the operator's own
public key so *they* can read every tip. The math is irrelevant if the code is
hostile, and a source has no way to audit what a normal website is running the
moment they load it.

Here the page is served by **attested code inside a hardware TEE**, reproducible
byte-for-byte from this repository via the on-chain catalog. A source can check
the attestation *before* trusting the page — confirm that the code serving the
form is exactly this code, that no operator can alter it, and that there is no
disk and no shell behind it. That is the property SecureDrop-style tools are
built around, and the property a web app normally cannot give. This one can.

## The trust math

- **Only you can read a tip.** Your ECDH P-256 keypair is generated in your
  browser. The public half rides the submission link; the private half rides
  *only* your owner link's URL *fragment* (`/i/<id>#k=…`), which browsers never
  send to any server. The enclave stores ciphertext under a line id you chose.
- **Even the sender can't read it back.** Each tip is sealed with a fresh
  *ephemeral* key that is discarded the instant it's sent (ECDH → AES-256-GCM).
  There is no copy anywhere that decrypts to the sender — forward secrecy
  against a later compromise of *their* device.
- **The notification path carries no ciphertext.** A landed tip pings your open
  inbox over SSE with only `{n, ts, size}`; your page fetches the sealed blob on
  demand, gated on your owner token. Guessing a topic reveals nothing.
- **Reads are gated, not guessable.** The inbox and the live stream require the
  owner token, checked against its stored SHA-256. Wrong token is a flat 403;
  an unknown line is a flat 404.
- **Nothing touches a disk.** State is enclave RAM and dies with the deployment.
  Logs carry counts only — never blobs, ids, or tokens.

## What the enclave still can't protect

Be clear-eyed about the edges the crypto doesn't reach:

- **Network metadata.** The enclave sees a tip's *arrival time* and *size*, and
  the sender's connection reveals their network position to the usual observers.
  If that matters, submit over Tor or a VPN — the page can't hide your IP.
- **Your own device.** Decryption happens in your browser. If your machine or
  your owner link is compromised, so are your tips. Guard the owner link.
- **Losing the key is permanent.** The private key exists *only* in your owner
  link. Lose it and every tip already received is unreadable forever — there is
  no reset and no backup. Anyone who gets the link reads everything.

## Features

- One public submission link per line; tips encrypted client-side with ephemeral
  ECDH P-256 → AES-256-GCM, discarded ephemeral keys for sender-side forward
  secrecy.
- Live owner inbox over SSE — a tip pings the page (no ciphertext on the wire),
  which refetches and decrypts in the browser; unread tips flash in the tab
  title. Per-tip copy and delete.
- Optional sender contact line, sealed inside the ciphertext like everything
  else.
- Public stats (`/api/stats`) — counts only, by construction.
- Caps: 64 KiB per tip, 200 tips per line (a ring, oldest dropped), 1 000 lines,
  48 MiB total; a line dies 30 days after its last tip. At capacity it says so
  rather than dropping someone's tip.

## API

Bodies are opaque base64url blobs or `k=v&` forms; JSON is emit-only. The server
never parses JSON, never generates randomness, never picks an id or a key.

| route | in | out |
|---|---|---|
| `POST /api/lines` | headers `x-line-id`, `x-owner-hash`, `x-pubkey`; body = title | `{ok}` · `400/409/507` |
| `GET /api/line?id=<id>` | — | PUBLIC `{title, pubkey, tips}` — count only, never the tips |
| `POST /api/tip` | `id=<id>&blob=<b64u>` | `{ok, n}` + SSE `tip` fan-out (no blob) |
| `GET /api/inbox?id=<id>&owner=<token>` | — | owner-only `{title, tips[], received, kept}` |
| `POST /api/tips/remove` | `id=&owner=&n=<seq>` | `{ok}` — owner deletes one tip |
| `GET /api/stream?id=<id>&owner=<token>` | `&s=<tag>?` | owner-verified SSE on topic `<id>` |
| `POST /api/leave` | `id=&s=<tag>` | `{ok}` — close a tagged stream (pagehide beacon) |
| `GET /api/stats` | — | counts only |
| `GET /` · `/t/<id>` · `/i/<id>` UI · `GET /ping` liveness | | |

## Design notes

Zero dependencies: `src/httpd.rs` is the suite's hand-rolled HTTP/1.1 + SSE
engine (one non-blocking event loop, the nanircd shape — wasip2 has no threads),
`src/sha256.rs` is FIPS 180-4 by hand (used to check the owner token against its
stored hash), and the whole UI is one embedded HTML file with inline WebCrypto.
The one platform rule, as ever: **read `ENCLAVE_PORTS`, bind the actual port** —
the deployment's `http:` entry is served at its origin by the enclave's in-TEE
TLS proxy.

## Build & test

```bash
rustup target add wasm32-wasip2        # or your distro's wasip2 std
cargo build --release --target wasm32-wasip2
# → target/wasm32-wasip2/release/tipline.wasm  (~205 KB component)

wasmtime run -Scli -Stcp -Sinherit-network -Sallow-ip-name-lookup \
  --env 'ENCLAVE_PORTS=http:8080=18092' \
  target/wasm32-wasip2/release/tipline.wasm
# then open http://127.0.0.1:18092
```

## Deploy on enclave.host

Publish the component (see the repo README / `guide` topic "publish"), then
deploy CPU-only — the minimum share is plenty:

```
enclave deploy <publisher>/tipline --cpu 0.01 --fund 2
```

Before pointing sources at a deployment, verify its attestation (guide topic
"attestation"): the entire premise is that a source doesn't have to take
anyone's word — including yours, including ours — for what the page is running.
```
