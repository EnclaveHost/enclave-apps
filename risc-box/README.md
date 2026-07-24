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
the guest-modified disk back out for saving) there are three functional
additions — a legacy virtio-net MMIO device at `0x10002000`, IRQ 2, with a
pluggable `NetBackend` mirroring the UART's `Terminal` (see Networking below);
ten missing RV64D float instructions (the FCVT int↔double family, FSGNJN.D,
FMIN/FMAX.D, FMSUB/FNMADD.D, FSQRT.D — busybox `ping` hits `FCVT.D.LU` on its
first timestamp); and unknown instructions now raise a proper
illegal-instruction trap (guest process gets SIGILL) instead of panicking the
whole host app — and six performance patches, measured end-to-end at 2.8×
throughput and 2.5× faster boot:

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
   guest curl/ping ──► smoltcp/NAT ──► real sockets ──► the internet
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
  "net": { "forwards": [ { "listen": 2222, "to": 22 } ] },
  "api_key": "$RISCBOX_API_KEY"
}
```

- `kernel` is an ELF with an SBI payload (OpenSBI `fw_payload`, or BBL+vmlinux);
  `fs` is a raw disk image mounted as `/dev/vda`. `dtb` is optional — the
  emulator ships a default device tree that boots the sample images.
- `saveKey` is where **Save disk** PUTs the guest-modified image (defaults to
  `fs`; set it aside to keep the pristine image). `readOnly: true` disables
  saving.
- Any string value in the config may be written as `$NAME` (or `${NAME}`):
  it is resolved from the app's **environment** at startup, which is where
  deployment secrets arrive. Whole-value references only. An unresolved
  reference logs a warning and reads as absent (so unresolved credentials
  fall back to the browser prompt). The config itself is read once, at
  process start: config or secret changes need a restart to take effect.
- The app **always starts**, even unconfigured. If a required field
  (`endpoint`/`bucket`/`kernel`/`fs`) is still empty — typically a `$VAR`
  secret you haven't set yet — it serves the UI and reports the gap in
  `/status` instead of exiting; it just refuses to boot a machine (a clear
  400 from `/start`) until the values are set and the process restarted. So a
  freshly deployed instance comes up ready to configure, not `failed`.
- `net` is optional: absent or `true` enables the guest NIC with the default
  forward (deployment port `tcp:2222` → guest `22`, made for sshd) and
  outbound NAT; `false` removes the network backend entirely; an object
  customizes both: `forwards` sets the port list, `"outbound": false` seals
  the machine to inbound-only (it can then exfiltrate nothing by itself). See
  Networking below.
- `api_key` is optional but **required for safety on a public deployment**:
  when set (use a `$VAR` secret, not a literal), every endpoint that drives or
  observes the machine — `/start`, `/stop`, `/save`, `/input`, `/console`,
  `/status` — demands it, presented as `Authorization: Bearer <key>`,
  `X-Api-Key: <key>`, or `?key=<key>` (the last for the SSE console). Only the
  static shell, its assets, and `/ping` stay open. See Security below.
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

### Outbound: user-mode NAT, slirp-style

The gateway also NATs the guest **outbound**, the way QEMU's user networking
(slirp) does. wasip2 has no raw sockets, so nothing is bridged — every guest
flow is re-terminated on a real socket that rides the platform's transparent
egress:

- **TCP** — a guest SYN to an external `ip:port` opens a real connection and
  splices it onto the guest's (same machinery as the inbound forwards). A
  refused or unreachable target answers the guest with an RST instead of a
  silent hang.
- **UDP** — one real socket per guest flow (capped at 64, idle-expired after
  60 s); replies are re-framed to the guest from the external source.
- **DNS** — the DHCP lease advertises `10.0.2.2` as resolver, and a proxy at
  `10.0.2.2:53` answers A queries with the platform's own name lookup (so
  resolution happens where the platform's egress policy lives; it works even
  where raw UDP egress does not). AAAA gets an empty NOERROR — the guest wire
  is IPv4-only, so dual-stack guests fall back cleanly.
- **ICMP echo** — `ping 8.8.8.8` works: the gateway answers echo requests
  itself, exactly like slirp. A reply confirms the NAT path is up, not that
  the target really answered an ICMP packet (none can leave the enclave).

In the guest, bring eth0 up (DHCP, or the static config above) and add the
gateway; then everything just dials out:

```sh
ifconfig eth0 10.0.2.15 netmask 255.255.255.0 up
route add default gw 10.0.2.2
mkdir -p /etc && echo 'nameserver 10.0.2.2' > /etc/resolv.conf

ping -c 3 8.8.8.8         # answered by the gateway
nslookup example.com      # resolved via the 10.0.2.2 proxy
wget http://example.com/  # TCP NAT (needs a working libc resolver, see below)
```

**Sample-image caveat:** the demo Buildroot images ship a *statically linked
glibc* busybox, whose `getaddrinfo` cannot resolve names on **any** network
(it needs NSS shared libraries that aren't in the image) — `wget` by hostname
says `bad address` without sending a single packet, under QEMU too. busybox
`nslookup` (its own resolver) works fine, as does any traffic by IP literal.
Real images with a dynamic libc (musl or full glibc) resolve normally.

`"outbound": false` in the `net` config removes all of this — the sealed,
inbound-only posture where the machine cannot exfiltrate anything by itself.
`/status` reports the network state under `net` (guest IP, forwards, frame
counters, active connections, `outbound`, and live `natTcp`/`natUdp` flow
counts).

Two honest notes on the TCP path: the one blocking step is the real
`connect()` (wasip2 has no async connect), bounded at 2.5 s — a guest dialing
a dead IP stalls the machine that long, once per attempt (up to 32 concurrent
outbound connections). And after any network activity the emulator runs
~100 M instructions at full speed before re-entering the idle throttle, so
interactive flows (ping's 1 s cadence, TCP handshakes) stay at wall-clock
pace instead of stretching with the idle clock.

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
wasmtime run -Stcp -Sudp -Sinherit-network -Sallow-ip-name-lookup \
  --env ENCLAVE_PORTS=http:8000=8000,tcp:2222=2222 --env RISCBOX_CONFIG="$CFG" \
  target/wasm32-wasip2/release/risc-box.wasm
```

(`-Sudp` is what lets the outbound UDP NAT open real sockets locally;
`-Sallow-ip-name-lookup` backs the DNS proxy.)

Open `http://127.0.0.1:8000/`, press **Boot machine**, and a RISC-V Linux
boots to a shell in about four seconds. The verification driven over this rig
covered: a SigV4 GET of a 9.9 MB kernel + 52 MB rootfs from minio, the boot
reaching a shell, an interactive command typed in the browser reaching the
guest and echoing back over SSE, a file written inside the guest and — after
**Save disk** — found byte-for-byte inside the **52 MB SigV4 PUT** image in
the bucket, a script written and then executed inside the guest (the
self-modifying-code path), and a wake-up round-trip after a long idle
(throttled) stretch. [`scripts/bench.py`](scripts/bench.py) replays all of
it. The outbound NAT was verified on the same rig from inside the guest
shell: `ping -c 3 8.8.8.8` (3/3 replies in 0.7 s wall), `nslookup` through
the `10.0.2.2` proxy and directly against `8.8.8.8` (UDP NAT), an HTTP body
fetched from the real internet over the TCP splice, and a dial to a closed
port answered with a fast RST.

## Caveats, honestly

- **User-mode network, one guest IP (10.0.2.15).** Outbound is NAT at the
  gateway, not a bridge: TCP and UDP flows work, `ping` is answered by the
  gateway itself, and exotic protocols (GRE, SCTP, traceroute's ICMP
  errors...) don't exist. Set `net.outbound: false` for a sealed machine.
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

## Security

The control surface drives a real machine: `/input` is a raw console into the
guest (a root shell, once one is running), and `/start` / `/stop` / `/save`
boot it, halt it, or write its disk back to your bucket. On a **public**
deployment those endpoints are reachable by anyone with the URL, so set
`api_key` (from a `$VAR` secret): it gates `/start`, `/stop`, `/save`,
`/input`, `/console`, and `/status`, leaving only the static shell and `/ping`
open. The browser UI prompts for the key and remembers it for the tab. Without
`api_key`, deploy **private** — an open deployment hands a stranger the
machine. The key is a coarse app-level gate, not the trust boundary; the
enclave is (see below).

Credentials passed at `/start` (rather than baked in the config) live only in
enclave RAM for that boot and never touch the disk image or the bucket
listing. The startup log states plainly whether S3 requests are **SIGNED** or
**UNSIGNED**, so a failing boot is easy to read: `UNSIGNED` next to an S3 4xx
means the credential secret never resolved (unset or misnamed); a `401` on a
`SIGNED` request means the resolved key/secret is wrong (e.g. a rotated token).

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
