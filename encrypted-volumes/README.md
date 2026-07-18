# encrypted-volumes — the encrypted-volumes app (rclone crypt over S3)

Encrypted volumes are user-held-key confidential storage: you encrypt a
directory **client-side** with [rclone crypt](https://rclone.org/crypt/) and
push the ciphertext to any S3-compatible bucket. The deployment's App Config
names *where* the ciphertext lives — never a key. At runtime the platform
preopens an empty dir per volume at `/enc/<name>` and starts the app
immediately; this app serves the **decryption UI**: the key enters in the
browser (over the deployment's in-enclave-terminated TLS), the app relays it
over loopback to the in-enclave manager (`ENCLAVE_ENC_API`, authenticated
with `ENCLAVE_ENC_TOKEN` — which never reaches the browser), and the manager
pulls + decrypts into the live preopen. The bucket, the network, and the
operator's host only ever see ciphertext; plaintext exists only on the CVM's
encrypted ramdisk.

## Wallet unlock (the primary flow)

**Unlock with wallet** derives the volume key from a deterministic ECDSA
`personal_sign` — no password to keep, no transaction, nothing stored or
verified on any server. The wallet signs a canonical message naming the
volume, and password + salt fall out of the signature; only the wallet
holder can reproduce them. The backend is untouched by this: the manager
still receives an opaque password, so any other derivation (or a plain
password) works identically.

The byte-exact contract, shared by this app's JS and
`scripts/enclave-encvol.sh` (`message` / `derive` subcommands, pinned by
`test/encvol-e2e.py` stage 3):

```
message  = "Enclave encrypted volume key v1\nvolume: <keyId>\n\n
            Signing derives this volume's encryption key. Only sign in
            apps you trust with its contents."        (single string, two \n\n)
sig      = lowercase 65-byte personal_sign hex, 0x-prefixed
password = sha256_hex( sig + "\n" + "enclave-encvol-v1:password" )
salt     = sha256_hex( sig + "\n" + "enclave-encvol-v1:salt" )
```

`keyId` defaults to the volume name; set it in the config if you want to
rename a volume without changing what the wallet signs (renaming otherwise
derives a different key). Config `unlock: "wallet"` makes the UI lead with
the wallet button (`"password"` leads with the form; both always work).
The app's **derive push credentials** panel signs and reveals the
signature/password/salt plus a ready `enclave-encvol.sh push` command for
seeding the bucket — or sign anywhere else:
`cast wallet sign "$(scripts/enclave-encvol.sh message myvol)"`.

Caveats: needs an injected EOA wallet (`window.ethereum`) that signs
deterministically (RFC 6979 — MetaMask, Ledger, EOAs generally);
smart-contract / ERC-1271 wallets and randomized MPC signers won't derive a
stable key — use password mode for those. Anyone holding the signature (or
who gets the wallet to sign this exact message) can derive the key: the
sign prompt is the security boundary.

The point for app authors: **your code needs no crypto and no S3 client.**
After unlock, `/enc/<name>` is an ordinary directory (`ENCLAVE_ENC` lists the
names) and plain `std::fs` works.

## Routes

| route                  | what                                                        |
|------------------------|-------------------------------------------------------------|
| `GET /`                | decryption UI + volume browser                              |
| `GET /api/status`      | proxied manager status (adds the token server-side)         |
| `POST /api/unlock`     | `{name, password, salt?, accessKeyId?, secretAccessKey?, sessionToken?}` |
| `POST /api/sync`       | `{name}` — push local edits back to the bucket (re-encrypted) |
| `POST /api/lock`       | `{name}` — wipe plaintext, drop retained credentials        |
| `GET /ls`              | JSON listing of every `ENCLAVE_ENC` volume                  |
| `GET /f/<vol>/<path>`  | raw file bytes (streamed; any size)                         |
| `GET /ping`            | liveness                                                    |

## Deploy it for real

```sh
# 1. encrypt + upload a directory (wraps rclone; prints the matching App
#    Config snippet when done). Wallet mode: sign the canonical message
#    (in this app's "derive push credentials" panel, or cast wallet sign)
#    and pass the signature - or omit --sig to be prompted for a password.
AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=… \
  scripts/enclave-encvol.sh push ./secret-dir --sig 0x… \
  --endpoint https://s3.eu-central-1.wasabisys.com --bucket my-bucket --path vols/demo --name demo

# 2. publish/deploy this app with the printed config as the version's App Config:
#    { "encVolumes": [ { "name": "demo", "unlock": "wallet",
#        "endpoint": "https://s3.eu-central-1.wasabisys.com",
#        "bucket": "my-bucket", "path": "vols/demo" } ] }

# 3. open the deployment in the browser and Unlock with wallet
#    (+ S3 credentials if the bucket isn't public-read).
```

Config knobs per volume: `filenameEncryption` (`standard`/`off`/`obfuscate`)
and `directoryNameEncryption` must match how the data was pushed; `maxMb`
caps the decrypted size on the enclave's ramdisk (default 1024); `readOnly:
true` makes the manager drop the credentials right after the pull, disabling
`/api/sync` push-back.

Trust notes: rclone crypt authenticates file *contents* (NaCl secretbox),
so the bucket can't tamper with what you read — but there is no signed
manifest of the tree, so a malicious bucket could serve a stale or partial
*set* of files. Prefer `https` endpoints; S3 credentials sent at unlock ride
attested TLS into the enclave and live only in enclave RAM.

## Try it locally

```sh
cargo component build --release --target wasm32-wasip2
wasmtime serve --addr 127.0.0.1:8080 -Scli -Shttp \
  --dir ./some-plaintext-dir::/enc/demo --env ENCLAVE_ENC=demo \
  target/wasm32-wasip*/release/encrypted_volumes.wasm
# (without ENCLAVE_ENC_API the /api routes 503; the browser + /f still work)
```

The full-stack e2e (real manager + real rclone + this app, plus a real-S3
stage against `rclone serve s3`) lives in the platform repo:
`python3 test/encvol-e2e.py`.
