# anima — a real machine on the enclave's CPU, booted from S3

anima boots a full operating system **inside the enclave**, the way QEMU
installed on a server would — the emulated CPU runs on the TEE's own silicon,
not in your browser. The enclave pulls a kernel and root filesystem from an
**S3 bucket**, boots them, and bridges the machine's serial console to your
browser; your keystrokes go back into the guest, and disk writes can be saved
back to the bucket.

This is the counterpart to [golem](../golem). golem ships QEMU-wasm to the
browser and emulates in the tab (the enclave is just the sealed image vault).
anima is the opposite split, the one the request asked for: **the machine
executes in the enclave.** That difference drives the whole design.

## Why an emulator, not "QEMU on the enclave"

QEMU's own WebAssembly port only runs in a browser — it is an Emscripten
build that needs JS glue, Web Workers, and SharedArrayBuffer, none of which
exist under a server-side wasm runtime. The platform runs apps as
`wasm32-wasip2` components under wasmtime, so a browser-targeted QEMU can
never execute there. Running a machine *in* the enclave therefore means a
system emulator that is itself a native `wasm32-wasip2` program.

anima vendors one:
[takahirox/riscv-rust](https://github.com/takahirox/riscv-rust) — a pure-Rust
RISC-V system emulator (RV64GC, Sv39 MMU, CLINT + PLIC + 16550 UART + virtio
block) that boots real Linux. It compiles to the same target as the rest of
the fleet and steps instruction-by-instruction in the TEE. The source lives
in [`emu/`](emu/), byte-for-byte upstream except two additive, in-line-tagged
patches (`dump_contents()` / `get_disk()`) that read the guest-modified disk
back out for saving.

## Architecture

anima is a run-mode **service app** — `wasmtime run` + `wasi:sockets`, one
attested process holding the machine in the enclave's RAM (the same shape as
[IRC](../IRC) and the utility suite; it reuses the suite's
[`httpd.rs`](src/httpd.rs) HTTP/1.1 + SSE engine). A single thread interleaves
the two jobs a machine host has:

- **be the CPU** — step a batch of guest instructions, drain the UART's output
  into a console broadcast, feed queued keystrokes into the UART's receive
  register;
- **be the front end** — accept HTTP, stream the console over Server-Sent
  Events, take input and control commands.

Images come from S3 over the platform's transparent egress.
[`src/s3.rs`](src/s3.rs) is a self-contained client: rustls with the pure-Rust
RustCrypto provider (the only TLS stack that builds for `wasm32-wasip2`) for
`https://`, a plain socket for `http://`, path-style requests, and SigV4
signing (GET to fetch, PUT to save) hand-rolled from `sha2`/`hmac` — no
`aws-sdk`, no `chrono`.

```
 S3 bucket ──GET(SigV4)──►  enclave: riscv-rust emulator (wasm32-wasip2)
 (kernel+rootfs)                       │  steps RV64 Linux on the TEE CPU
       ▲                               │  UART ⇄ SSE / POST /input
       └──PUT(SigV4)── save disk ──────┘         │
                                           your browser (xterm.js console)
```

## Configuration

The deployment's App Config (`ENCLAVE_CONFIG`; locally, `ANIMA_CONFIG`) is a
JSON object:

```json
{
  "title": "Buildroot (RISC-V)",
  "endpoint": "https://s3.eu-central-1.wasabisys.com",
  "region": "eu-central-1",
  "bucket": "my-bucket",
  "kernel": "images/fw_payload.elf",
  "fs": "images/rootfs.img",
  "dtb": "images/board.dtb",
  "saveKey": "images/rootfs.img",
  "credentials": { "accessKeyId": "...", "secretAccessKey": "...", "sessionToken": "..." },
  "autostart": false,
  "readOnly": false
}
```

- `kernel` is an ELF with an SBI payload (OpenSBI `fw_payload`, or BBL+vmlinux);
  `fs` is a raw disk image mounted as `/dev/vda`. `dtb` is optional — the
  emulator ships a default device tree that boots the sample images.
- `saveKey` is where **Save disk** PUTs the guest-modified image (defaults to
  `fs`; set it aside to keep the pristine image). `readOnly: true` disables
  saving.
- **Credentials** are optional. A public-read bucket needs none (requests go
  unsigned). Otherwise, credentials may sit in the config (the enclave attests
  it) **or** be typed in the browser at boot — they are sent only to this app,
  over the deployment's in-enclave-terminated TLS, and live only in enclave
  RAM. `autostart: true` boots at process start (needs a public bucket or
  config credentials).

## Routes

| route             | what                                                                 |
|-------------------|----------------------------------------------------------------------|
| `GET /`           | console UI (self-contained HTML + embedded xterm)                    |
| `GET /a/<asset>`  | embedded `xterm.js` / `xterm.css`                                    |
| `GET /status`     | JSON: phase, image sizes, instructions retired, MIPS, console bytes  |
| `POST /start`     | `{accessKeyId?,secretAccessKey?,sessionToken?,reset?}` — fetch from S3 and boot; `reset:true` re-fetches instead of using the cached images |
| `POST /input`     | **raw bytes** in the body → the guest UART receive register          |
| `GET /console`    | Server-Sent Events: base64 console output, scrollback replayed first |
| `POST /save`      | dump the guest disk and PUT it to `saveKey`                          |
| `POST /stop`      | halt the machine and drop it from RAM                                |
| `GET /ping`       | liveness                                                             |

## Seeding a bucket

[`scripts/seed-machine.py`](scripts/seed-machine.py) is a stdlib-only companion
(its SigV4 mirrors `src/s3.rs`). Fetch a ready-made RISC-V sample and upload it:

```sh
scripts/seed-machine.py fetch-sample ./images
scripts/seed-machine.py put --endpoint https://s3.… --region … --bucket my-bucket \
    --access-key AKIA… --secret-key … ./images/fw_payload.elf images/fw_payload.elf
scripts/seed-machine.py put … ./images/rootfs.img images/rootfs.img
```

The sample is the OpenSBI + Linux + Buildroot image set from the vendored
emulator's own resources. Any RISC-V kernel/rootfs that boots on the
`virt`-style machine works — build your own with Buildroot and drop them in.

## Try it locally

Against [minio](https://min.io) standing in for S3 (this is exactly the rig the
app was verified on):

```sh
cargo build --release --target wasm32-wasip2

# 1. an S3 to boot from
minio server /tmp/anima-data --address 127.0.0.1:9100 &
# (create a bucket + upload images/fw_payload.elf and images/rootfs.img;
#  seed-machine.py, mc, or any S3 client does this)

# 2. anima under wasmtime, with the service-app socket grants + config
CFG='{"title":"demo","endpoint":"http://127.0.0.1:9100","region":"us-east-1",
      "bucket":"machines","kernel":"images/fw_payload.elf","fs":"images/rootfs.img",
      "saveKey":"images/rootfs.saved.img",
      "credentials":{"accessKeyId":"…","secretAccessKey":"…"}}'
wasmtime run -Stcp -Sinherit-network -Sallow-ip-name-lookup \
  --env ENCLAVE_PORTS=http:8000=8000 --env ANIMA_CONFIG="$CFG" \
  target/wasm32-wasip2/release/anima.wasm
```

Open `http://127.0.0.1:8000/`, press **Boot machine**, and a RISC-V Linux
boots to a login shell in roughly ten seconds. The verification driven over
this rig covered: a SigV4 GET of a 9.9 MB kernel + 52 MB rootfs from minio, the
boot reaching a shell, an interactive command typed in the browser reaching the
guest and echoing back over SSE, and a **52 MB SigV4 PUT** of the guest disk to
the bucket (confirmed present and correctly sized).

## Caveats, honestly

- **RISC-V RV64 only, one hart.** anima runs what the vendored emulator runs:
  a single-core RISC-V `virt`-style machine. Not x86, not multi-core.
- **Emulated speed.** The interpreter turns tens of MIPS under wasmtime — fine
  for a shell, a build, a demo; not a fast VM. There is no KVM in a TEE wasm
  sandbox; this is pure interpretation.
- **Blocking image load.** Fetching the images (tens of MB over TLS) happens
  in the event loop, so the console briefly stalls for other clients during a
  boot. One-time, a few seconds.
- **Save consistency.** Save copies the disk while the guest runs; run `sync`
  (or halt) in the guest first, or the snapshot is crash-consistent at best.
  Only the disk is saved — not live RAM, so this is not a suspend/resume.
- **RAM budget.** The machine's memory and both images live in enclave RAM
  (the emulator sizes guest DRAM at 128 MiB); size the deployment accordingly.
- **Credentials.** If the bucket needs credentials and you don't seal them in
  the attested config, they are entered per boot and held only in RAM (a Stop,
  or a restart, drops them).

## Trust notes

The machine, its images, and any typed credentials exist only inside the
enclave; the bucket and the host operator see S3 traffic, not the running
guest. anima authenticates nothing about the *images themselves* beyond the
bucket's own integrity — a malicious bucket could serve a different kernel, so
treat the bucket as part of your trust base and prefer `https` endpoints.
Confirm the code that runs your machine and handles your credentials with the
deployment's remote attestation at
[enclave.host](https://enclave.host).

The vendored emulator is MIT ([`emu/LICENSE`](emu/LICENSE)); anima itself is
MIT. It is a faithful RISC-V emulator, not a security boundary — the enclave
is the boundary.
