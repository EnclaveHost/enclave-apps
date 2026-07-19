# keep — a notebook only your wallet can open

keep is a confidential notebook backed by a **wallet-unlocked encrypted
volume**. Your notes live encrypted (rclone crypt) in your own S3 bucket; the
bucket, the network, and the operator's host only ever see ciphertext. You
unlock in the browser — a single wallet signature derives the volume key — and
the in-enclave manager decrypts the notes onto the CVM's encrypted ramdisk.
From there keep reads and writes them with plain `std::fs`: no crypto in the
app, no S3 client, no key on disk.

## Why this needs an encrypted volume

A notebook wants three things at once that a stateless app can't give you:

- **Persistent** — notes survive restarts and redeploys, because the durable
  copy is ciphertext in your bucket, not enclave RAM.
- **Private** — the plaintext only ever exists inside the enclave, after you
  unlock. The host it runs on never sees a key or a cleartext byte.
- **Ordinary to write** — once unlocked, `/enc/<name>` is just a directory.
  keep's server is ~200 lines of `std::fs`; the volume does the hard part.

That's the encrypted-volumes model (see the sibling `encrypted-volumes` app),
extended from a read-only browser into a read-**write** editor.

## The Save / Sync / Lock lifecycle

keep is honest about where your bytes are at every moment:

| action   | what happens                                                                 |
|----------|------------------------------------------------------------------------------|
| **Save** | writes the note as plaintext into the enclave's in-memory ramdisk (`std::fs::write`). Fast, but volatile — a Lock or a restart wipes it. |
| **Sync** | asks the manager to **re-encrypt** the current volume and **push the ciphertext** to your bucket. This is what makes an edit durable. |
| **Lock** | wipes the decrypted plaintext from the ramdisk and drops the retained S3 credentials. The ciphertext in your bucket is untouched. |

The editor shows a live dirty-state indicator — *unsaved changes* → *saved*
(on ramdisk) → *synced* (pushed to your bucket) — so you always know whether an
edit has actually left the enclave.

## Routes

| route                  | what                                                        |
|------------------------|-------------------------------------------------------------|
| `GET /`                | the notebook UI (self-contained HTML)                       |
| `GET /api/status`      | proxied manager status (adds the bearer token server-side)  |
| `POST /api/unlock`     | `{name, password, salt?, accessKeyId?, secretAccessKey?, sessionToken?}` |
| `POST /api/sync`       | `{name}` — re-encrypt local edits and push to the bucket     |
| `POST /api/lock`       | `{name}` — wipe plaintext, drop retained credentials         |
| `POST /api/write`      | `{vol, path, content}` — write a note (`std::fs`, ≤ 1 MiB)   |
| `POST /api/delete`     | `{vol, path}` — remove a note                                |
| `POST /api/mkdir`      | `{vol, path}` — create a directory                           |
| `GET /ls`              | JSON listing of every `ENCLAVE_ENC` volume                  |
| `GET /f/<vol>/<path>`  | raw file bytes (streamed)                                   |
| `GET /ping`            | liveness                                                    |

`write` / `delete` / `mkdir` validate every path through the same segment
filter as `/f/` — no `..`, no absolute jumps, no volumes outside `ENCLAVE_ENC`
— and `write` caps a note at 1 MiB. The unlock/sync/lock routes are relayed
verbatim to the manager's loopback `/encvol` plane; the bearer token
(`ENCLAVE_ENC_TOKEN`) is attached server-side and never reaches the browser.

## Using it for real

keep on its own is the front door — full use needs **your own S3 bucket seeded
with rclone-crypt ciphertext**, exactly as the `encrypted-volumes` sample
describes. The short version:

1. Encrypt and upload a directory of starter notes (or an empty one) with the
   `scripts/enclave-encvol.sh push` flow documented in the `encrypted-volumes`
   README. In **wallet mode** you sign the canonical message (keep's *derive
   push credentials* panel prints a ready `enclave-encvol.sh push` command, or
   use `cast wallet sign`) and pass the signature; the same signature that
   seeds the bucket is the one that later unlocks it.
2. Publish keep with that volume in the App Config:
   ```json
   { "encVolumes": [ { "name": "notes", "unlock": "wallet",
       "endpoint": "https://s3.eu-central-1.wasabisys.com",
       "bucket": "my-bucket", "path": "vols/notes" } ] }
   ```
   Optionally seal S3 credentials under the same signature (keep's **Seal**
   button, or `enclave-encvol.sh seal-creds`) and add the printed
   `"credsEnvelope"` — then a wallet unlock needs nothing typed, on any
   restart.
3. Open the deployment, **Unlock with wallet**, and write.

Because a volume is read-write, keep leaves `readOnly` off; the manager retains
the write-capable S3 credentials so **Sync** can push back. Set `"readOnly":
true` in the config if you want a look-but-don't-touch deployment (Sync is then
disabled).

## The wallet is the only key

There is no account, no password reset, no server-side copy of anything. The
volume key is derived from your wallet's signature over a fixed message; the
S3 `credsEnvelope`, if you use one, opens under that same signature. **Lose the
wallet and you lose the notes** — the ciphertext in the bucket becomes
undecryptable, by you or anyone. Anyone who can get that wallet to sign this
exact message can derive the key, so the sign prompt is the security boundary:
only sign in keep, over a deployment you've verified. Deterministic-ECDSA EOA
wallets (MetaMask, Ledger) work; smart-contract / MPC signers don't derive a
stable key — use a password volume for those.

## Try it locally

```sh
cargo build --release --target wasm32-wasip2
wasmtime serve -Scommon --addr 127.0.0.1:8080 \
  --dir ./some-plaintext-dir::/enc/notes --env ENCLAVE_ENC=notes \
  target/wasm32-wasip2/release/keep.wasm
# Save/Delete/Mkdir and /f work against the preopen with no manager.
# Point ENCLAVE_ENC_API + ENCLAVE_ENC_TOKEN at a manager for unlock/sync/lock.
```

Verify the enclave before trusting it — [enclave.host](https://enclave.host).
