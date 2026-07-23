# RISC Box — a real machine on the enclave's CPU, booted from S3

RISC Box boots a full operating system **inside the enclave**, the way QEMU
installed on a server would — the emulated CPU runs on the TEE's own silicon,
not in your browser. The enclave pulls a kernel and root filesystem from an
**S3 bucket**, boots them, and bridges the machine's serial console to your
browser; your keystrokes go back into the guest, and disk writes can be saved
back to the bucket.

This is the counterpart to [golem](../golem). golem ships QEMU-wasm to the
browser and emulates in the tab (the enclave is just the sealed image vault).
RISC Box is the opposite split, the one the request asked for: **the machine
executes in the enclave.** That difference drives the whole design.

## Why an emulator, not "QEMU on the enclave"

QEMU's own WebAssembly port only runs in a browser — it is an Emscripten
build that needs JS glue, Web Workers, and SharedArrayBuffer, none of which
exist under a server-side wasm runtime. The platform runs apps as
`wasm32-wasip2` components under wasmtime, so a browser-targeted QEMU can
never execute there. Running a machine *in* the enclave therefore means a
system emulator that is itself a native `wasm32-wasip2` program.

RISC Box vendors one:
[takahirox/riscv-rust](https://github.com/takahirox/riscv-rust) — a pure-Rust
RISC-V system emulator (RV64GC, Sv39 MMU, CLINT + PLIC + 16550 UART + virtio
block) that boots real Linux. It compiles to the same target as the rest of
the fleet and steps instruction-by-instruction in the TEE. The source lives
in [`emu/`](emu/); every divergence from upstream is tagged `risc-box patch`
in-line. Beyond the original two (`dump_contents()` / `get_disk()`, which read
the guest-modified disk back out for saving) there is one functional addition
(a legacy virtio-net MMIO device at `0x10002000`, IRQ 2, with a pluggable
`NetBackend` mirroring the UART's `Terminal`; see Networking below) and six
performance patches, measured end-to-end at 2.8× throughput and 2.5× faster
boot:

- a direct-mapped **software TLB** in front of the Sv32/Sv39 page walk,
  tagged with a generation counter plus the translation-relevant CPU state;
  satp/mode changes and `SFENCE.VMA` (a no-op upstream — honored here)
  invalidate in O(1), and entries are filled only by walks that already set
  the PTE A/D bits, so hits never touch the page table;
- a **predecoded instruction cache** keyed by virtual PC: on a hit, one tag
  compare replaces fetch translation, memory read, RVC uncompression, and
  decode. Self-modifying code is handled properly — pages backing cached
  instructions are marked, and every DRAM write (CPU store *or* virtio DMA;
  both funnel through one wrapper) that touches a marked page invalidates
  the cache by generation bump;
- the LRU decode cache (hash map + linked list on every hit) replaced by a
  **direct-mapped decode table** — one shift, one mask, one compare;
- misaligned memory access **two-cell paths** replacing per-byte loops
  (compressed instructions put half of all fetches at `pc % 4 == 2`), which
  also fixes an upstream bug corrupting 4-aligned misaligned 8-byte loads;
- the cycle CSR materialized **lazily on read** instead of written every tick;
- `Cpu::is_idle()`, so the host can throttle a guest parked in WFI.

## Architecture

RISC Box is a run-mode **service app** — `wasmtime run` + `wasi:sockets`, one
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
       └──PUT(SigV4)── save disk ──────┤         │
                                       │   your browser (xterm.js console)
              ssh/tcp ──tcp:2222──► smoltcp ⇄ virtio-net ⇄ guest eth0 :22
```

## Configuration

The deployment's App Config (`ENCLAVE_CONFIG`; locally, `RISCBOX_CONFIG`) is a
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
  "readOnly": false,
  "net": { "forwards": [ { "listen": 2222, "to": 22 } ] }
}
```

- `kernel` is an ELF with an SBI payload (OpenSBI `fw_payload`, or BBL+vmlinux);
  `fs` is a raw disk image mounted as `/dev/vda`. `dtb` is optional — the
  emulator ships a default device tree that boots the sample images.
- `saveKey` is where **Save disk** PUTs the guest-modified image (defaults to
  `fs`; set it aside to keep the pristine image). `readOnly: true` disables
  saving.
- `net` is optional: absent or `true` enables the guest NIC with the default
  forward (deployment port `tcp:2222` → guest `22`, made for sshd); `false`
  removes the network backend entirely; an object with `forwards` customizes
  the port list. See Networking below.
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

## Networking and SSH

The guest gets a **virtio-net NIC** (eth0). There is no bridge to a real
network: the app terminates the guest's ethernet in user space with
[smoltcp](https://github.com/smoltcp-rs/smoltcp) (`src/net.rs`), which plays
the LAN at `10.0.2.2/24`, answers DHCP with a static lease for `10.0.2.15`,
and splices **inbound TCP forwards** from the deployment's raw `tcp:` ports
onto guest connections. The default forward is `tcp:2222` → guest `22`:

```sh
ssh -p 2222 root@<deployment-host>        # reaches sshd inside the guest
```

For that to answer, two things must be true in your image:

- **an sshd is installed and running**: the sample Buildroot image has none
  (verify the path with busybox `nc -l -p 22` instead); build your own with
  Buildroot (`BR2_PACKAGE_DROPBEAR=y`) or any distro image with openssh;
- **eth0 has its address**: a DHCP client on eth0 gets the lease
  (busybox `udhcpc -i eth0` needs its `/usr/share/udhcpc/default.script`
  present, which minimal images often omit), or configure it statically:

```sh
ifconfig eth0 10.0.2.15 netmask 255.255.255.0 up
```

The deployment must declare the forward ports alongside http:
`ports="http:8000,tcp:2222"` at publish time; the app resolves the actual
bind via `ENCLAVE_PORTS` exactly like the http port.

**No outbound NAT (yet).** The guest can be reached through forwards and can
talk to 10.0.2.2, but cannot open connections out to the internet; the
machine cannot exfiltrate anything by itself, which keeps the trust story
simple. `/status` reports the network state under `net`
(guest IP, forwards, frame counters, active connections).

## Try it locally

Against [minio](https://min.io) standing in for S3 (this is exactly the rig the
app was verified on):

```sh
cargo build --release --target wasm32-wasip2

# 1. an S3 to boot from
minio server /tmp/riscbox-data --address 127.0.0.1:9100 &
# (create a bucket + upload images/fw_payload.elf and images/rootfs.img;
#  seed-machine.py, mc, or any S3 client does this)

# 2. RISC Box under wasmtime, with the service-app socket grants + config
CFG='{"title":"demo","endpoint":"http://127.0.0.1:9100","region":"us-east-1",
      "bucket":"machines","kernel":"images/fw_payload.elf","fs":"images/rootfs.img",
      "saveKey":"images/rootfs.saved.img",
      "credentials":{"accessKeyId":"…","secretAccessKey":"…"}}'
wasmtime run -Stcp -Sinherit-network -Sallow-ip-name-lookup \
  --env ENCLAVE_PORTS=http:8000=8000,tcp:2222=2222 --env RISCBOX_CONFIG="$CFG" \
  target/wasm32-wasip2/release/risc-box.wasm
```

Open `http://127.0.0.1:8000/`, press **Boot machine**, and a RISC-V Linux
boots to a shell in about four seconds. The verification driven over this rig
covered: a SigV4 GET of a 9.9 MB kernel + 52 MB rootfs from minio, the boot
reaching a shell, an interactive command typed in the browser reaching the
guest and echoing back over SSE, a file written inside the guest and — after
**Save disk** — found byte-for-byte inside the **52 MB SigV4 PUT** image in
the bucket, a script written and then executed inside the guest (the
self-modifying-code path), and a wake-up round-trip after a long idle
(throttled) stretch. [`scripts/bench.py`](scripts/bench.py) replays all of
it.

## Caveats, honestly

- **Inbound network only.** The guest NIC reaches the in-app user-mode
  network: DHCP, the forwards, nothing outbound. One guest IP (10.0.2.15).
- **RISC-V RV64 only, one hart.** RISC Box runs what the vendored emulator runs:
  a single-core RISC-V `virt`-style machine. Not x86, not multi-core.
- **Emulated speed.** The interpreter turns ~29 MIPS under wasmtime (software
  TLB + predecoded instruction cache; measured on the sample image) — fine
  for a shell, a build, a demo; not a fast VM. There is no KVM in a TEE wasm
  sandbox; this is pure interpretation. An idle guest parked in WFI is
  throttled to ~1–2% host CPU; keystrokes force full-speed batches so the
  console stays snappy.
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
guest. RISC Box authenticates nothing about the *images themselves* beyond the
bucket's own integrity — a malicious bucket could serve a different kernel, so
treat the bucket as part of your trust base and prefer `https` endpoints.
Confirm the code that runs your machine and handles your credentials with the
deployment's remote attestation at
[enclave.host](https://enclave.host).

The vendored emulator is MIT ([`emu/LICENSE`](emu/LICENSE)); RISC Box itself is
MIT. It is a faithful RISC-V emulator, not a security boundary — the enclave
is the boundary.
