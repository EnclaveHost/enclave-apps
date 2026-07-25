# shoebox — a wallet-unlocked private file locker on Enclave

A shoebox under the bed for the cloud era: drop your photos, scans, receipts,
recordings, and PDFs into a private locker whose contents decrypt **only
inside the enclave**. The ciphertext lives in your own S3 bucket; the bucket
operator and the host machine only ever see encrypted blobs. You open the
locker by signing one message with your wallet — there is no password to store
and nothing to verify on any server. Inside, shoebox is a plain
upload / download / delete gallery: images render as thumbnails, everything
else gets a file tile, and a drag-and-drop zone adds more.

shoebox is built on Enclave's first-party
[`encrypted-volumes`](../encrypted-volumes) sample and speaks the exact same
manager contract. It is a **`wasi:http` component**: the platform preopens an
empty directory per volume at `/enc/<name>` and starts the app immediately;
the app serves the decryption UI, relays the key over loopback to the
in-enclave encryption manager, and — once unlocked — reads and writes the
volume with ordinary `std::fs`. Your code needs no crypto and no S3 client.

## The lifecycle of a locker

1. **Seed the bucket (once, off-enclave).** You encrypt a directory
   client-side with [rclone crypt](https://rclone.org/crypt/) and push the
   ciphertext to any S3-compatible bucket, using the reference app's
   `scripts/enclave-encvol.sh push`. The wallet signature (or a password)
   derives the crypt key; the bucket never learns it. This step is the only
   way ciphertext gets into the bucket — **shoebox cannot bootstrap an empty
   bucket for you**; it unlocks and edits what `push` seeded.

2. **Deploy shoebox** with an App Config `encVolumes` entry naming *where* the
   ciphertext lives (endpoint, bucket, path) — never a key.

3. **Unlock.** Open the deployment and **Unlock with wallet**. The wallet signs
   a fixed message naming the volume; the volume key falls out of the
   signature, so only the wallet holder can reproduce it. The manager pulls the
   ciphertext from the bucket and decrypts it onto the CVM's encrypted ramdisk.
   The files appear in the gallery.

4. **Edit.** Drag files in (each up to 8 MiB), download them back, delete what
   you don't want. Every change is plaintext on the enclave ramdisk and shows
   an **unsynced changes** marker.

5. **Sync** re-encrypts the current locker and pushes ciphertext back to your
   bucket (needs write-capable S3 credentials at unlock, and a volume not
   marked `readOnly`). **Lock** wipes the plaintext from the enclave and drops
   the retained credentials; the ciphertext in the bucket is untouched.

**The wallet is the only key.** Nobody — not the host operator, not the bucket
provider, not Enclave — can decrypt the locker without the signature. Lose the
wallet and the ciphertext is unrecoverable; that is the point.

## One-signature unlock (credentials envelope)

S3 credentials can ride the **public** App Config sealed under the same wallet
signature that derives the crypt key, as `credsEnvelope`. Then a single
signature both derives the key and opens the bucket credentials — nothing typed
at unlock, after any restart. Seal them from the app's **derive push
credentials → Seal** panel (or `enclave-encvol.sh seal-creds`) and paste the
printed value into the volume's config. The envelope is exactly as sensitive as
the volume: the wallet that signs for the key is the wallet that opens it.

Wallet caveats carry over from the reference: you need an injected EOA wallet
(`window.ethereum`) that signs deterministically (RFC 6979 — MetaMask, Ledger,
EOAs generally). Smart-contract / ERC-1271 wallets and randomized MPC signers
won't derive a stable key — use password mode for those.

## Routes

| route                     | what                                                          |
|---------------------------|--------------------------------------------------------------|
| `GET /`                   | the locker UI (self-contained HTML)                          |
| `GET /api/status`         | proxied manager status (adds the bearer token server-side)   |
| `POST /api/unlock`        | `{name, password, salt?, accessKeyId?, secretAccessKey?, sessionToken?}` |
| `POST /api/sync`          | `{name}` — re-encrypt local edits and push to the bucket     |
| `POST /api/lock`          | `{name}` — wipe plaintext, drop retained credentials         |
| `POST /api/delete`        | `{vol, path}` — remove a file from the volume                |
| `POST /up/<vol>/<path>`   | **raw file bytes** in the body → written into the volume (8 MiB cap) |
| `GET /ls`                 | JSON listing of every `ENCLAVE_ENC` volume                   |
| `GET /f/<vol>/<path>`     | raw file bytes (streamed; any size), typed by extension      |
| `GET /ping`               | liveness                                                     |

`/up` and `/api/delete` are shoebox's additions; everything else is the
reference contract verbatim. The bearer token (`ENCLAVE_ENC_TOKEN`) is attached
server-side and never reaches the browser.

## Try it locally

The full recipe (mock manager + `wasmtime serve`) mirrors the reference app:

```sh
cargo build --release --target wasm32-wasip2
wasmtime serve -Scommon --dir ./some-plaintext-dir::/enc/demo \
  --env ENCLAVE_ENC=demo --env ENCLAVE_ENC_API=http://127.0.0.1:PORT \
  --env ENCLAVE_ENC_TOKEN=... --addr 127.0.0.1:8080 \
  target/wasm32-wasip2/release/shoebox.wasm
```

Without `ENCLAVE_ENC_API` the `/api/*` routes answer 503, but `/`, `/ls`,
`/f`, `/up`, and `/api/delete` all work against the preopened directory — a
plain, unencrypted locker you can poke at directly.

## Deploy it for real

Use the reference app's `scripts/enclave-encvol.sh` to seed the bucket, then
publish shoebox with the printed config as the version's App Config:

```json
{ "encVolumes": [ { "name": "demo", "unlock": "wallet",
    "endpoint": "https://s3.eu-central-1.wasabisys.com",
    "bucket": "my-bucket", "path": "vols/demo" } ] }
```

Per-volume config knobs match the reference: `filenameEncryption` /
`directoryNameEncryption` must match how the data was pushed; `maxMb` caps the
decrypted size on the ramdisk; `readOnly: true` disables push-back (Sync).

## Trust notes

rclone crypt authenticates file *contents* (NaCl secretbox), so the bucket
can't tamper with what you read — but there is no signed manifest of the tree,
so a malicious bucket could serve a stale or partial *set* of files. Prefer
`https` endpoints; S3 credentials sent at unlock ride the deployment's
in-enclave-terminated TLS into the enclave and live only in enclave RAM.
Confirm the code that holds your files with the deployment's remote attestation
at [enclave.host](https://enclave.host).
