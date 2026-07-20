# golem — QEMU machines from a sealed volume, on Enclave

A machine room where the machines are clay until you speak the word: your OS
images live **encrypted in your own S3 bucket**, decrypt **only inside the
enclave** when your wallet signs, and boot as full Linux systems with
[QEMU compiled to WebAssembly](https://github.com/ktock/qemu-wasm) — the
emulated CPU runs **in your browser tab**, fed file-by-file by the attested
enclave. Snapshot the guest disk back into the volume, Sync it to your
bucket, and carry a whole VM around as ciphertext.

Be clear about the split, because it is the honest heart of the design:
**QEMU executes in the browser, not in the enclave.** QEMU-wasm is an
Emscripten build — it needs JS glue, Web Workers, and SharedArrayBuffer, which
no server-side wasm runtime provides (the platform runs `wasm32-wasip2`
components under wasmtime). What the enclave *is*, is the part a browser
can't be and a bucket won't be:

- the **sealed image vault** — machine images exist as plaintext only on the
  CVM's encrypted ramdisk, unlocked by your wallet signature; the bucket, the
  network, and the host operator only ever see rclone-crypt ciphertext;
- the **cross-origin-isolated origin** — SharedArrayBuffer requires
  COOP/COEP response headers on every load. Static S3 hosting can't set
  them; golem sets them on every byte it serves;
- the **attested code path** — remote attestation pins exactly what serves
  the unlock UI and handles your crypt key, which no CDN offers;
- the **writable, private return path** — guest-disk snapshots stream back
  in chunks, land next to the images, and re-encrypt to your bucket on Sync.

golem is built on Enclave's first-party
[`encrypted-volumes`](../encrypted-volumes) sample and speaks the exact same
manager contract as [keep](../keep) and [shoebox](../shoebox): a `wasi:http`
component; the platform preopens `/enc/<name>` per volume; unlock relays the
key over loopback to the in-enclave manager (`ENCLAVE_ENC_API`, authenticated
with `ENCLAVE_ENC_TOKEN` — never sent to the browser).

## The lifecycle of a machine

1. **Assemble a machine (once, off-enclave).** No Emscripten toolchain
   needed — `scripts/fetch-machine.sh` downloads a prebuilt QEMU-wasm bundle
   plus guest image from the upstream
   [qemu-wasm-demo](https://github.com/ktock/qemu-wasm-demo) pages and writes
   the `machine.json` manifest:

   ```sh
   scripts/fetch-machine.sh alpine-x86_64 ./machines   # ~145 MB
   scripts/fetch-machine.sh raspi3ap ./machines        # ~85 MB, aarch64
   ```

2. **Seed the bucket.** Encrypt + push with the reference app's script, key
   derived from your wallet signature:

   ```sh
   ENCVOL_WALLET_SIG=0x… scripts/enclave-encvol.sh push ./machines \
     --endpoint https://s3.… --bucket my-bucket --path vols/golem --name golem
   ```

3. **Deploy golem** with an App Config naming *where* the ciphertext lives —
   never a key:

   ```json
   { "encVolumes": [ { "name": "golem", "unlock": "wallet",
       "endpoint": "https://s3.eu-central-1.wasabisys.com",
       "bucket": "my-bucket", "path": "vols/golem",
       "maxMb": 2048 } ] }
   ```

   Size `maxMb` for images **plus snapshots** (an Alpine machine is ~140 MB;
   each snapshot of its disk is ~150 MB). S3 credentials can ride the config
   sealed as `credsEnvelope` (seal them from the app's *derive push
   credentials* panel) for one-signature unlock.

4. **Unlock.** Open the deployment, **Unlock with wallet**. The manager pulls
   and decrypts your machines onto the enclave ramdisk; the machine room
   lists every directory containing a `machine.json`.

5. **Boot.** The page loads the QEMU bundle and image packs from the enclave
   (ETag-revalidated, so reboots hit the browser cache), and the serial
   console goes live in an xterm. Alpine reaches `golem login:` in ~30 s.

6. **Save.** Run `sync` in the guest so the disk is quiescent, then **Save
   disk to volume** — the guest disk streams out of the emulator into
   `<machine>/saves/<label>.img` in 8 MiB chunks. **Boot this save** starts
   the machine from a snapshot instead of the pristine image. **Sync**
   re-encrypts the volume (snapshots included) to your bucket; **Lock** wipes
   the plaintext.

**The wallet is the only key.** Lose it and the ciphertext is unrecoverable;
that is the point.

## machine.json

A machine is any directory in the volume with a `machine.json`:

```json
{
  "title": "Alpine Linux (x86_64)",
  "main": "out.js",
  "loaders": ["load-rom.js", "load-kernel.js", "load-initramfs.js", "load-rootfs.js"],
  "diskLoader": "load-rootfs.js",
  "disk": "/pack-rootfs/disk-rootfs.img",
  "args": ["-nographic", "-M", "pc", "-m", "512M", "…"]
}
```

- `main` — the Emscripten ES6 module of the QEMU build; `args` — its QEMU
  command line, verbatim.
- `loaders` — classic scripts loaded first, in order: the
  `file_packager` packs that carry BIOS/kernel/initramfs/disk into the
  emulator's in-memory FS. They honor `Module.locateFile`, so golem points
  them at `/f/<volume>/<machine>/…`.
- `disk` — the emulator-FS path of the guest disk (what Save snapshots).
- `diskLoader` — which loader carries *only* the disk. Booting a snapshot
  skips it and writes the saved image over `disk` in `preRun` instead. If the
  disk shares a pack with the kernel (raspi3ap), omit it: snapshots can be
  saved but not booted directly.

Bring your own machines: rebuild image packs with Emscripten's
`file_packager.py`, or build QEMU-wasm yourself (upstream's
`create-images.sh` shows both). QEMU is GPLv2 — the fetched bundles are
built from [ktock/qemu-wasm](https://github.com/ktock/qemu-wasm).

## Routes

| route                          | what                                                            |
|--------------------------------|-----------------------------------------------------------------|
| `GET /`                        | the machine room UI (self-contained HTML)                       |
| `GET /a/<name>`                | embedded xterm.js / xterm-pty / css (immutable-cached)          |
| `GET /api/status`              | proxied manager status (token attached server-side)             |
| `POST /api/{unlock,sync,lock}` | proxied to the manager, reference contract                      |
| `POST /api/delete`             | `{vol, path}` — delete a file (used for snapshots)              |
| `POST /up/<vol>/<path>?off=N`  | raw chunk (≤16 MiB) written at byte offset N; `off=0` truncates; wrong offset → 409 `{expected}` |
| `GET /ls`                      | JSON listing of every `ENCLAVE_ENC` volume                      |
| `GET /f/<vol>/<path>`          | file bytes — streamed, `ETag`/`If-None-Match`, single `Range`/`If-Range`, typed by extension |
| `GET /ping`                    | liveness                                                        |

Every response carries `Cross-Origin-Opener-Policy: same-origin`,
`Cross-Origin-Embedder-Policy: require-corp`,
`Cross-Origin-Resource-Policy: same-origin` and nosniff — the isolation that
makes `crossOriginIsolated` true and SharedArrayBuffer (QEMU's threads)
available.

## Try it locally

```sh
cargo build --release --target wasm32-wasip2
scripts/fetch-machine.sh alpine-x86_64 ./machines
python3 scripts/mock-manager.py &          # fake always-unlocked manager on :8391
wasmtime serve -Scommon --dir ./machines::/enc/demo \
  --env ENCLAVE_ENC=demo --env ENCLAVE_ENC_API=http://127.0.0.1:8391 \
  --env ENCLAVE_ENC_TOKEN=test --addr 127.0.0.1:8390 \
  target/wasm32-wasip2/release/golem.wasm
```

Open `http://127.0.0.1:8390/` in Chromium and press **Boot**. This exact rig
(driven headless over CDP) is how the app was verified: cross-origin
isolation on, Alpine to `login:` in ~30 s, a 150 MiB snapshot saved through
`/up`, and a boot from that snapshot — all against the real prebuilt bundle.

## Caveats, honestly

- **Browser support.** Tested on Chromium (as is upstream QEMU-wasm). The
  emulator preallocates a ~2.3 GB SharedArrayBuffer; Firefox/Safari may
  refuse or crawl.
- **Snapshot consistency.** Save copies the disk while the guest runs — run
  `sync` (or `poweroff`) in the guest first; a mid-write snapshot is a
  crash-consistent disk at best.
- **No guest networking yet.** Machines boot with `-nic none`. Upstream's
  in-browser fetch-proxy stack (c2w-net-proxy) is loadable through the same
  volume mechanism and is the natural next step.
- **RAM is the budget.** Volume plaintext lives on the enclave ramdisk and
  each snapshot is a full disk image; size `maxMb` accordingly and delete
  stale saves.
- **Big first load.** ~145 MB streams enclave→browser on first boot;
  after that, ETag revalidation makes reboots near-instant.

## Trust notes

rclone crypt authenticates file contents, so the bucket can't tamper with
the machine you boot — but there is no signed manifest of the *tree*: a
malicious bucket could serve a stale set (e.g. resurrect an old snapshot).
The wallet key derivation, envelope format, and manager contract are
byte-exact with the reference app. Verify the code that holds your images —
and that serves the page which handles your signature — with the
deployment's remote attestation at
[enclave.host](https://enclave.host).
