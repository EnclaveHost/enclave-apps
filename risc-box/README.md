# RISC Box: a real machine on the enclave's CPU, booted from S3

RISC Box boots a full operating system **inside the enclave**, the way QEMU
installed on a server would: the emulated CPU runs on the TEE's own silicon,
not in your browser. The enclave pulls a kernel and root filesystem from an
**S3 bucket**, boots them, and bridges the machine's serial console to your
browser; your keystrokes go back into the guest, and disk writes can be saved
back to the bucket.

This is the counterpart to [golem](../golem). golem ships QEMU-wasm to the
browser and emulates in the tab (the enclave is just the sealed image vault).
RISC Box is the opposite split, the one the request asked for: **the machine
executes in the enclave.** That difference drives the whole design.

## Why an emulator, not "QEMU on the enclave"

QEMU's own WebAssembly port only runs in a browser; it is an Emscripten
build that needs JS glue, Web Workers, and SharedArrayBuffer, none of which
exist under a server-side wasm runtime. The platform runs apps as
`wasm32-wasip2` components under wasmtime, so a browser-targeted QEMU can
never execute there. Running a machine *in* the enclave therefore means a
system emulator that is itself a native `wasm32-wasip2` program.

RISC Box vendors one:
[takahirox/riscv-rust](https://github.com/takahirox/riscv-rust), a pure-Rust
RISC-V system emulator (RV64GC, Sv39 MMU, CLINT + PLIC + 16550 UART + virtio
block) that boots real Linux. It compiles to the same target as the rest of
the fleet and steps instruction-by-instruction in the TEE. The source lives
in [`emu/`](emu/); every divergence from upstream is tagged `risc-box patch`
in-line. Beyond the original two (`dump_contents()` / `get_disk()`, which read
the guest-modified disk back out for saving) there are three functional
additions: a legacy virtio-net MMIO device at `0x10002000`, IRQ 2, with a
pluggable `NetBackend` mirroring the UART's `Terminal` (see Networking below);
the missing float instructions, ten RV64D (the FCVT int↔double family,
FSGNJN.D, FMIN/FMAX.D, FMSUB/FNMADD.D, FSQRT.D; busybox `ping` hits
`FCVT.D.LU` on its first timestamp), then `FCLASS.D` (glibc's
`fpclassify`/`isnan` compile to it) and the whole single-precision RV64F set
(upstream had almost none of it; Xorg SIGILLs on `FSGNJ.S` at startup);
`mstatus.FS` reported **always Dirty** so the guest kernel saves/restores FP
registers on every context switch (the emulator doesn't track f-register
writes, and a Clean FS made Linux skip the save while still restoring the
zeroed save area; any process holding live FP values across a syscall got
them silently wiped); unknown instructions now raise a proper
illegal-instruction trap (guest process gets SIGILL) instead of panicking the
whole host app; unmapped physical addresses read as open bus (0, writes
dropped and rate-limit logged) instead of panicking; virtio-blk reports the
disk's real capacity (was hardcoded 100 MiB; ext4 saw "bad geometry") and
drains its whole avail ring per queue notify (one buffer per notify hung
large-filesystem mounts in `io_schedule`). Plus six performance patches,
measured end-to-end at 2.8× throughput and 2.5× faster boot:

- a direct-mapped **software TLB** in front of the Sv32/Sv39 page walk,
  tagged with a generation counter plus the translation-relevant CPU state;
  satp/mode changes and `SFENCE.VMA` (a no-op upstream, honored here)
  invalidate in O(1), and entries are filled only by walks that already set
  the PTE A/D bits, so hits never touch the page table;
- a **predecoded instruction cache** keyed by virtual PC: on a hit, one tag
  compare replaces fetch translation, memory read, RVC uncompression, and
  decode. Self-modifying code is handled properly: pages backing cached
  instructions are marked, and every DRAM write (CPU store *or* virtio DMA;
  both funnel through one wrapper) that touches a marked page invalidates
  the cache by generation bump;
- the LRU decode cache (hash map + linked list on every hit) replaced by a
  **direct-mapped decode table**: one shift, one mask, one compare;
- misaligned memory access **two-cell paths** replacing per-byte loops
  (compressed instructions put half of all fetches at `pc % 4 == 2`), which
  also fixes an upstream bug corrupting 4-aligned misaligned 8-byte loads;
- the cycle CSR materialized **lazily on read** instead of written every tick;
- `Cpu::is_idle()`, so the host can throttle a guest parked in WFI.

## Architecture

RISC Box is a run-mode **service app**: `wasmtime run` + `wasi:sockets`, one
attested process holding the machine in the enclave's RAM (the same shape as
[IRC](../IRC) and the utility suite; it reuses the suite's
[`httpd.rs`](src/httpd.rs) HTTP/1.1 + SSE engine). A single thread interleaves
the two jobs a machine host has:

- **be the CPU**: step a batch of guest instructions, drain the UART's output
  into a console broadcast, feed queued keystrokes into the UART's receive
  register;
- **be the front end**: accept HTTP, stream the console over Server-Sent
  Events, take input and control commands.

Images come from S3 over the platform's transparent egress.
[`src/s3.rs`](src/s3.rs) is a self-contained client: rustls with the pure-Rust
RustCrypto provider (the only TLS stack that builds for `wasm32-wasip2`) for
`https://`, a plain socket for `http://`, path-style requests, and SigV4
signing (GET to fetch, PUT to save) hand-rolled from `sha2`/`hmac`; no
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
  "ramMiB": 512,
  "display": { "width": 1024, "height": 768 },
  "realtime": false,
  "net": { "forwards": [ { "listen": 2222, "to": 22 } ] },
  "api_key": "$RISCBOX_API_KEY",
  "snapshot": "images/desktop.snap",
  "restoreExec": "date -s @{epoch} >/dev/null; echo {entropy} > /dev/urandom"
}
```

- `kernel` is an ELF with an SBI payload (OpenSBI `fw_payload`, or BBL+vmlinux);
  `fs` is a raw disk image mounted as `/dev/vda`. `dtb` is optional; the
  emulator ships a default device tree that boots the sample images.
- `saveKey` is where **Save disk** PUTs the guest-modified image (defaults to
  `fs`; set it aside to keep the pristine image). `readOnly: true` disables
  saving (and snapshotting).
- `instances` — `{"max": 8, "maxBytes": 3221225472}` bounds how many machines
  this process hosts (`main` included) and the host memory they may add up to;
  see *Many machines, one process*.
- `snapshot` names the object a **snapshot of the booted machine** lives under.
  When it exists, `/start` resumes the machine from it in seconds instead of
  booting the OS; when it does not exist yet, the boot is a cold one and
  `POST /snapshot` writes it (to `snapshotSaveKey`, default `snapshot`) so every
  start after that is instant. `snapshotLevel` (1-9, default 2) is the deflate
  level. `restoreExec` is a shell command run on the guest console right after a
  resume; `{epoch}` and `{entropy}` expand to the host's UNIX time and 64 fresh
  random bytes (hex). See *Instant boot from a snapshot*.
- Any string value in the config may be written as `$NAME` (or `${NAME}`):
  it is resolved from the app's **environment** at startup, which is where
  deployment secrets arrive. Whole-value references only. An unresolved
  reference logs a warning and reads as absent (so unresolved credentials
  fall back to the browser prompt). The config itself is read once, at
  process start: config or secret changes need a restart to take effect.
- The app **always starts**, even unconfigured. If a required field
  (`endpoint`/`bucket`/`kernel`/`fs`) is still empty, typically a `$VAR`
  secret you haven't set yet, it serves the UI and reports the gap in
  `/status` instead of exiting; it just refuses to boot a machine (a clear
  400 from `/start`) until the values are set and the process restarted. So a
  freshly deployed instance comes up ready to configure, not `failed`.
- `net` is optional: absent or `true` enables the guest NIC with the default
  forward (deployment port `tcp:2222` → guest `22`, made for sshd) and
  outbound NAT; `false` removes the network backend entirely; an object
  customizes both: `forwards` sets the port list, `"outbound": false` seals
  the machine to inbound-only (it can then exfiltrate nothing by itself). See
  Networking below.
- `display` sets the simple-framebuffer's resolution (default 1024x768; must be
  even and fit the DTB's 3 MiB window). The emulator rewrites the device tree
  node and the app scans out the same shape, so the two can never disagree. It
  is worth setting, because **every pixel is paid for three times**: the guest
  draws it, the scan reads it back, the encoder ships it. A machine whose job is
  a 320x200 game spends nine tenths of that work on borders at 1024x768. The
  browser scales whatever it is given to a 4:3 window, so a small framebuffer is
  cheap to run without being small to look at.
- `realtime` runs the guest's clock off the **host's monotonic clock** instead
  of off retired instructions (default false; see *What time it is inside*).
- `api_key` is optional but **required for safety on a public deployment**:
  when set (use a `$VAR` secret, not a literal), every endpoint that drives or
  observes the machine (`/start`, `/stop`, `/save`, `/input`, `/console`,
  `/status`) demands it, presented as `Authorization: Bearer <key>`,
  `X-Api-Key: <key>`, or `?key=<key>` (the last for the SSE console). Only the
  static shell, its assets, and `/ping` stay open. See Security below.
- **Credentials** are optional. A public-read bucket needs none (requests go
  unsigned). Otherwise, credentials may sit in the config (the enclave attests
  it) **or** be typed in the browser at boot; they are sent only to this app,
  over the deployment's in-enclave-terminated TLS, and live only in enclave
  RAM. `autostart: true` boots at process start (needs a public bucket or
  config credentials).

## Routes

| route             | what                                                                 |
|-------------------|----------------------------------------------------------------------|
| `GET /`           | console UI (self-contained HTML + embedded xterm)                    |
| `GET /a/<asset>`  | embedded `xterm.js` / `xterm.css`                                    |
| `GET /status`     | JSON: phase, image sizes, instructions retired, MIPS, frames presented (`fps`) and shipped (`sentFps`), display mode, console bytes |
| `POST /start`     | `{accessKeyId?,secretAccessKey?,sessionToken?,reset?,snapshot?}`: fetch from S3 and boot — or resume from the snapshot when one is cached; `reset:true` re-fetches instead of using the cached images, `snapshot:false` forces a cold boot |
| `POST /input`     | **raw bytes** in the body → the guest UART receive register          |
| `POST /exec`      | `{cmd, timeout_s?, max_bytes?}`: run a shell command on the guest console and return its stdout + exit code as JSON (see below) |
| `GET /console`    | Server-Sent Events: base64 console output, scrollback replayed first |
| `POST /save`      | dump the guest disk and PUT it to `saveKey`                          |
| `POST /snapshot`  | `{key?,level?}`: serialize the running machine and PUT it to the snapshot key; later starts resume from it |
| `GET /instances`  | the machines this process hosts, the images they fork from, the memory in use |
| `POST /instances` | `{from?: "main" \| "<snapshot key>", id?}`: fork a new machine (default: the config's root snapshot); created running |
| `/i/<id>/…`       | an instance's `status`, `console`, `input`, `exec`, `hid`, `fb.png`, `frame.jpg`, `fb.rgb`, `snapshot`, `stop`, `start`; `DELETE /i/<id>` forgets it |
| `POST /stop`      | halt the machine and drop it from RAM                                |
| `GET /ping`       | liveness                                                             |

## Running commands: `POST /exec`

`/input` and `/console` are the raw serial line — bytes in, bytes out. `/exec`
is the verb built on top of them for a **program** driving the machine (another
enclave app, a script), where "type this, wait, scrape the output" is exactly
what you do not want to reimplement. It runs one shell command on the guest and
answers with its stdout and exit code:

```sh
curl -s -XPOST http://<host>/exec \
  -d '{"cmd":"apk info | wc -l; uname -a","timeout_s":30}'
# {"ok":true,"exitCode":0,"output":"57\nLinux … riscv64 …","truncated":false,"ms":412}
```

Body: `cmd` (required), `timeout_s` (1–120, default 30, covers login **and** the
command), `max_bytes` (output cap, default 64 KiB). It answers `{"ok":true,
"exitCode":N,"output":"…","truncated":bool,"ms":N}`, or `{"ok":false,"error":
"…","output":"<whatever came back>"}` on a timeout or a console that never
reached a prompt. `409` when the machine is not running, `403` when exec is
disabled.

How it works, so its limits are legible: there is no exec channel inside the
guest, so this drives the serial console the way [`scripts/bench.py`](scripts/bench.py)
does, server-side. It first sends a newline and waits for a shell prompt —
answering a `login:` from the configured credentials (passwordless `root` by
default) — so a getty is logged into automatically and an already-open shell is
used as-is. Then it writes the command **base64-wrapped** (so any bytes, quotes
and newlines survive) bracketed by two `printf` markers whose tag is a printf
*argument*, so the command line the tty echoes back never contains the expanded
marker and cannot be mistaken for the real output. stdout is everything printed
between the markers; the exit code rides the closing one.

Consequences worth knowing: the call **blocks the event loop** until the command
finishes or times out (the same way `/start`'s image fetch does), so keep
`timeout_s` well under the platform gateway's ~180 s idle cut; the guest UART
accepts ~one byte per 230 k instructions, so a very long command line takes a
moment to land; and it needs a shell on `ttyS0` — an image that boots straight
to a desktop with no serial getty has nothing to exec into. It adds **no new
authority** over `/input` (both are a root console) and sits behind the same
`api_key` gate. Configure the login and an off-switch with an `exec` block:

```json
"exec": { "enabled": true, "user": "root", "password": "$GUEST_ROOT_PW" }
```

### Driving the machine from another enclave app (e.g. eyesoff-ai)

Because `/exec` is a plain request/response JSON API, an app whose only outbound
door is `wasi:http` can call it as a tool with **no code** — for eyesoff-ai, one
`tools.http` entry in the deployment config:

```json
{
  "name": "run_vm_command",
  "description": "Run a shell command on the Alpine Linux virtual machine and get its stdout and exit code. Use it to inspect or operate the machine: list files, read logs, install packages, edit files, run programs. One independent command per call; keep state on the machine (files, not shell variables) between calls.",
  "parameters": {
    "type": "object",
    "properties": { "cmd": { "type": "string", "description": "the shell command to run" } },
    "required": ["cmd"]
  },
  "url": "https://<riscbox-id8>.app.enclave.host/exec",
  "method": "POST",
  "headers": { "x-api-key": "$RISCBOX_API_KEY" },
  "body": { "cmd": "$cmd", "timeout_s": 60 },
  "timeout_s": 70
}
```

The model calls it mid-answer, reads the JSON back, and can call again — the
tool loop's `max_calls` bounds a run-look-run sequence. Deployment notes:

- **Present the key as `X-Api-Key`, not `Authorization: Bearer`.** The platform's
  TLS proxy consumes the `Authorization` header for its own owner-token auth and
  never forwards it, so a Bearer token from another app (or a browser) arrives
  stripped and the request 401s. `X-Api-Key` and `?key=` pass through; the app
  accepts all three (`authorized()`), but only the latter two survive the proxy.
- Because eyesoff reaches risc-box over the public origin, the risc-box
  deployment must be **public with an `api_key` set** (the `$RISCBOX_API_KEY`
  secret above), or the proxy refuses the call before the app sees it.
- Outbound egress is IPv6-only, but `*.app.enclave.host` publishes an AAAA, so
  app-to-app works. Set eyesoff's per-tool `timeout_s` above risc-box's so the
  model sees a real error rather than a truncated connection.

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
`virt`-style machine works. Build your own with Buildroot and drop them in.

### Gzip your rootfs

**Name the object `.gz` and it is fetched, cached and saved compressed.** Guest
disks are mostly empty, so this is not a marginal saving: the XFCE image is
320 MiB raw and 53 MiB gzipped, a 6.3x cut.

```sh
gzip -6 rootfs.ext2                       # -> rootfs.ext2.gz
scripts/seed-machine.py put … ./rootfs.ext2.gz xfce/rootfs.ext2.gz
#   "fs": "xfce/rootfs.ext2.gz"
```

It buys two different things. The fetch **blocks the event loop** — the
machine, the console and every other client wait on it — so six times less to
download is six times less of the boot spent stalled. And the fetched bytes are
held for the lifetime of the app so a restart need not re-download; cached
compressed, that copy costs 53 MiB instead of 320 MiB, beside a running machine
that already holds the expanded disk plus its DRAM. On a 2 GiB deployment that
headroom is the difference between comfortable and not.

Saving follows the name: `saveKey` falls back to the `fs` key, so a machine
booted from a `.gz` image writes its disk back as real gzip rather than raw
bytes under a `.gz` name — which would boot exactly once more and then fail
forever with "bad magic". The output is ordinary gzip, `gunzip -t`-clean CRC
and all, so the bucket stays readable with normal tools.

## Instant boot from a snapshot

A desktop guest takes the emulated core a couple of minutes to bring up: OpenSBI,
the kernel, init, Xorg, the session. None of that work depends on anything but
the images, so it only has to be done once. `POST /snapshot` serializes the
**running machine** — CPU registers and CSRs, every device's registers and
rings, the device tree as the guest saw it, guest RAM, and the disk blocks the
guest has changed — and PUTs it to the configured `snapshot` key. From then on
every `/start`, including the one after a restart or a redeploy, **resumes** the
machine from that moment: fetch, inflate, continue. Locally the sample image
restores in well under a second; on the fleet the cost is the download.

```sh
# 1. deploy with "snapshot": "images/desktop.snap" in the config — the object
#    does not exist yet, so the first start boots cold
# 2. once the machine is where you want it (desktop up, logged in, quiet):
curl -s -XPOST -H "x-api-key: $KEY" https://<id8>.app.enclave.host/snapshot
# {"ok":true,"key":"images/desktop.snap","bytes":13924085,"ramPagesKept":7751,
#  "ramPagesTotal":131072,"deltaBlocks":1,"snapshotMs":320,"uploadMs":1400}
# 3. every start from now on resumes:
curl -s -XPOST -H "x-api-key: $KEY" .../stop; curl -s -XPOST -H "x-api-key: $KEY" .../start -d '{}'
curl -s -H "x-api-key: $KEY" .../status | jq .snapshot
# {"key":"images/desktop.snap","cachedBytes":13924085,"restored":true,"restoreMs":812,...}
```

What is in it, and what is not. Guest RAM is written sparsely — zero pages are
elided, the rest deflated — so a booted 512 MiB machine is a few tens of MB.
The disk is **not** copied: the snapshot carries only the blocks the guest
wrote since the base image was loaded, and a restore fetches the `fs` object
exactly as a cold boot does and lays the delta over it. That is what keeps a
snapshot small, and it is also why a snapshot is **bound to its images**: it
records a sha256 of the kernel and fs objects as fetched (plus the RAM size,
display geometry and clock mode), and a start whose images or settings differ
ignores the snapshot with a log line and boots cold rather than mount a
filesystem half from another image. Overwrite the base image, or change
`ramMiB`, and you take a new snapshot. A snapshot is also bound to the
emulator's own format (`emu/src/snapshot.rs`, `FORMAT`): a newer app that
changed a device's layout refuses old snapshots the same way.

What a resumed guest does not know. It believes it is the moment the snapshot
was taken: its wall clock is stale, and its kernel random pool is the same one
every other resume of this snapshot has. `restoreExec` runs a command on the
console right after the resume, through the same machinery as `/exec`, and
`{epoch}` / `{entropy}` expand to what the guest needs to fix both:

```json
"restoreExec": "date -s @{epoch} >/dev/null; echo {entropy} > /dev/urandom"
```

(It needs a shell on `ttyS0`, like `/exec`.) Any TCP connection the guest held
open at snapshot time — an ssh session, a download — was a real host socket and
is simply gone on resume; take the snapshot when the machine is quiet. The guest
NIC, its DHCP lease and the port forwards all carry over, because the address
plan is static.

`POST /start` with `{"snapshot": false}` boots the base images cold while a
snapshot exists (to build a new one, say); `{"reset": true}` re-fetches images
and snapshot both. `/status` reports the snapshot key, whether one is cached
and how big, whether the running machine was restored and how long that took.
The same machinery is in `boot-bench` for measuring without S3:
`--snapshot-on MARKER:FILE` writes one when MARKER appears on the console,
`--restore FILE` resumes from it.

## Many machines, one process

`main` is the machine the config describes. Beside it the app hosts
**instances**: machines forked from a snapshot image, each with its own RAM,
disk overlay, serial console, `/exec` and NIC. Think one root image and a
machine per user, or per chat — created in milliseconds, thrown away when the
conversation ends.

```sh
curl -s -XPOST -H "x-api-key: $KEY" .../instances -d '{}'            # fork the config's root snapshot
# {"id":"3fa9c1e2","origin":"images/desktop.snap","phase":"running","ramMiB":512,...}
curl -s -XPOST -H "x-api-key: $KEY" .../instances -d '{"from":"main","id":"scratch"}'
curl -s -XPOST -H "x-api-key: $KEY" .../i/3fa9c1e2/exec -d '{"cmd":"hostname; uptime"}'
curl -s -H "x-api-key: $KEY" .../i/3fa9c1e2/fb.png > screen.png
curl -s -XDELETE -H "x-api-key: $KEY" .../i/3fa9c1e2
curl -s -H "x-api-key: $KEY" .../instances | jq .summary
# {"count":3,"max":8,"footprintBytes":412090368,"maxBytes":3221225472,"images":2}
```

What makes this affordable is how a machine's memory is held. Guest RAM is
an array of 64 KiB chunks that start untouched (reads as zero, costs nothing),
are **shared** with the image the machine forked from, and are copied only
when the guest writes into them. The base disk is one shared object; each
machine keeps an overlay of the 4 KiB blocks it has written. So N instances of
one booted image cost one image plus what each has diverged since — a fresh
fork of a 512 MiB desktop costs a few megabytes, and a 4 GiB wasm32 process
can hold a room full of them. The footprint `/instances` reports is exactly
that: owned chunks, overlays, resident images, the base disk.

`from` names the image. The config's `snapshot` key is the default root
(inflated once, then resident); any other snapshot object in the bucket works
the same way; `"main"` takes the **live main machine as it is right now** —
its RAM becomes shared, the fork is a few milliseconds and never touches the
bucket — which is how to build an instance from a machine you have just set
up by hand. An instance's RAM size is the image's; its display geometry and
clock mode must match the deployment's, because those are part of the
identity a snapshot is bound to. The `restoreExec` hook runs on every fork
(a forked guest has the same stale clock and random pool a resumed one has).

The scheduler is round-robin: each turn every running machine that is not
parked in WFI gets an equal share of the turn's instruction budget, and an
idle guest costs the host almost nothing, so a hundred sleeping instances are
cheap and three busy ones each get a third of the core. `instances.max`
(default 8, `main` included) and `instances.maxBytes` (default 3 GiB) bound
the count and the memory; a fork past either is refused with a 409.

What stays with `main`: the deployment's port forwards (instances get outbound
NAT only), the display/video/audio streams, the GameStream host, `/save`. An
instance's `/stop` drops its pages; `/start` is a fresh fork of the same
origin, not a resume. An instance's `/snapshot` needs an explicit `key`.

## Past 4 GiB: the wasm64 build

A wasm32 component addresses 4 GiB of linear memory, and everything a box
holds lives there: the guest's RAM, the disk image, the framebuffer, the
fork roots. That caps a guest at roughly 1.9 GiB of RAM and a box at about
3 GiB of instances. `wasm64/build.sh` produces `risc-box64.wasm`, the same
app as a memory64 component: `ramMiB` may go to 65536, `instances.maxBytes`
defaults to 32 GiB, and the platform's memory ceiling for a memory64
component is the deployment's whole RAM slice.

There is no wasm64-wasip2 target in rustc, no wasm64 wasi-libc release, and
the engine's own host bindings still speak a 32-bit ABI, so the build is a
toolchain of its own (`wasm64/prepare-toolchain.sh`, or the Dockerfile next
to it) and the app runs on top of a wasm32 pass-through component that
forwards every WASI call across the engine's 64-to-32 adapter. All of it,
and why each piece exists, is in [docs/wasm64.md](docs/wasm64.md).

The guest kernel must be able to map the memory: the Alpine kernel from
`guest/` (Linux 5.15, Sv39) does; the sample image's 5.4 kernel stops at
2 GiB by its own config. Nothing else changes for the box's users: same
config, same routes, same snapshots (a snapshot's identity does not include the RAM size, so a
wasm32-made snapshot resumes on the wasm64 build). Run it locally with

    wasmtime run -W memory64,component-model-memory64 -W max-memory-size=$((12<<30)) \
      -Sinherit-network -Sallow-ip-name-lookup --env RISCBOX_CONFIG=... wasm64/risc-box64.wasm

and the lifecycle harness takes `--mem64` (see `scripts/snaptest.py`).

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
(slirp) does. wasip2 has no raw sockets, so nothing is bridged: every guest
flow is re-terminated on a real socket that rides the platform's transparent
egress:

- **TCP**: a guest SYN to an external `ip:port` opens a real connection and
  splices it onto the guest's (same machinery as the inbound forwards). A
  refused or unreachable target answers the guest with an RST instead of a
  silent hang.
- **UDP**: one real socket per guest flow (capped at 64, idle-expired after
  60 s); replies are re-framed to the guest from the external source.
- **DNS**: the DHCP lease advertises `10.0.2.2` as resolver, and a proxy at
  `10.0.2.2:53` answers A queries with the platform's own name lookup (so
  resolution happens where the platform's egress policy lives; it works even
  where raw UDP egress does not). AAAA gets an empty NOERROR; the guest wire
  is IPv4-only, so dual-stack guests fall back cleanly.
- **ICMP echo**: `ping 8.8.8.8` works; the gateway answers echo requests
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
(it needs NSS shared libraries that aren't in the image); `wget` by hostname
says `bad address` without sending a single packet, under QEMU too. busybox
`nslookup` (its own resolver) works fine, as does any traffic by IP literal.
Real images with a dynamic libc (musl or full glibc) resolve normally.

`"outbound": false` in the `net` config removes all of this: the sealed,
inbound-only posture where the machine cannot exfiltrate anything by itself.
`/status` reports the network state under `net` (guest IP, forwards, frame
counters, active connections, `outbound`, and live `natTcp`/`natUdp` flow
counts).

Two honest notes on the TCP path: the one blocking step is the real
`connect()` (wasip2 has no async connect), bounded at 2.5 s; a guest dialing
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
guest and echoing back over SSE, a file written inside the guest and, after
**Save disk**, found byte-for-byte inside the **52 MB SigV4 PUT** image in
the bucket, a script written and then executed inside the guest (the
self-modifying-code path), and a wake-up round-trip after a long idle
(throttled) stretch. [`scripts/bench.py`](scripts/bench.py) replays all of
it. The outbound NAT was verified on the same rig from inside the guest
shell: `ping -c 3 8.8.8.8` (3/3 replies in 0.7 s wall), `nslookup` through
the `10.0.2.2` proxy and directly against `8.8.8.8` (UDP NAT), an HTTP body
fetched from the real internet over the TCP splice, and a dial to a closed
port answered with a fast RST.

## What time it is inside

The device tree advertises a 10 MHz timebase, and `mtime` — the register behind
`rdtime`, the kernel's clocksource and every `gettimeofday` in the guest —
advances by one tick per retired instruction. That makes a boot deterministic,
which is exactly what the benchmark below wants. It also means **the guest's
second is not a second**: at the 130 MIPS this interpreter now retires, ten
million ticks pass in 77 ms, so the machine lives thirteen seconds for every one
of ours.

Nothing that only computes can tell. Everything that paces itself off the clock
can: `sleep 1` returns in 77 ms, a kernel takes thirteen times HZ timer
interrupts per real second, and a game runs at thirteen times speed while
looking perfectly correct from inside. It is also why an in-guest frame counter
cannot answer "how fast is this really" — it is measuring a ruler that stretches
with the thing it measures.

`realtime: true` swaps the source: `mtime` becomes the host's monotonic clock at
the same 10 MHz, resampled every few thousand instructions (a clock read per
instruction would cost more than the instruction). Then a guest second is a
second, `sleep 1` takes one, the kernel's timer interrupt rate drops by that
same factor of thirteen — real work, not just honesty — and a frame rate
measured in the guest means what it says.

The cost is determinism: with time coming from outside, interrupts land at
host-dependent points and the instruction count to reach a marker stops being
repeatable. So it is a knob, not a default, and `boot-bench --realtime` matches
it for measuring anything that involves time.

## A game, at the speed a game should run

The machine plays DOOM. Freedoom Phase 1, running on the framebuffer of a
RISC-V Linux that is itself an interpreter inside a TEE, at **35 frames per
second** — the rate the DOOM engine itself tops out at, since its world advances
35 tics a second and it has no frame to draw between them.

Getting there was not emulator work. Measured against the same guest, same
emulator, in the app's own wasm build:

| what the guest ran | instructions per frame | frames per real second |
|---|---|---|
| chocolate-doom, 640x480 window, X11 + fluxbox, 1024x768 screen | 12.6M | 14 |
| fbdoom straight to /dev/fb0, 320x200 screen | 1.6M | 35 (engine cap) |

Nearly eight times the per-frame cost was the X server, SDL's scaler and the
window manager — a chain in which the game's own renderer was the cheapest
link. DOOM draws 320x200; the X path then scaled that to 640x480, converted it,
pushed it through the X protocol, and had Xorg blit it into a 1024x768
framebuffer, which the app then had to scan and deflate in full. Deleting the
chain — a 400-line video backend that mmaps `/dev/fb0`, fuses the palette lookup
into the scale, and reads the keyboard straight off evdev — leaves the game
paying for its own pixels and nothing else.

Two numbers, not one, describe what a viewer gets, and `/status` reports both:
`fps` is what the guest presented (framebuffer bytes painted ÷ bytes per frame,
so it counts what actually reached the screen rather than what the guest claims)
and `sentFps` is what the scan shipped to watchers. On the single-threaded
build the second is the smaller one — the scan is paced by what it costs, and it
costs the same thread the guest runs on — which is precisely what the SET
worker below exists to fix.

## Watching the machine costs the machine

The app runs the guest, serves HTTP, scans the framebuffer and encodes video on
**one thread** — `wasm32-wasip2` cannot spawn another, on p2 or p3 (`std::thread`
returns "Not supported", `available_parallelism()` is 1). So a browser with the
page open is not a passive observer. Every frame it is sent is emulator time the
guest did not get.

Measured against a pinned workload (a busy loop in the guest, so its demand is
constant), alternating no-watcher / watcher / no-watcher so drift shows up as
disagreement between the no-watcher samples:

| | MIPS | cost |
|---|---|---|
| nobody watching | 36.3 | — |
| `/display` (deflated dirty bands, fixed 10 fps) | 34.2 | 6% |
| `/video` (AV1, fixed 10 fps) | 6.6 | **82%** |

At 82% a desktop that starts in four minutes takes twenty, and it looks broken
rather than slow. Both encoders are now paced by what they *cost* instead of by
the clock — after each frame they wait until at least four times as long has
passed not encoding — which puts AV1 at 20% and lets either speed up on an idle
machine and back off on a busy one.

For `/display` that budget alone would be a regression, because a still screen
makes a scan cheap and the budget would happily spend it looking for nothing
sixty times a second. So the scan also backs off while the picture holds still,
doubling its interval up to the old 100 ms, and snaps straight back to 16 ms
the moment a band comes out. A still desktop therefore costs what it always
did; a moving one gets up to six times the frame rate it used to.

**Prefer the `/display` stream.** It is a tenth the cost, and the AV1 toggle
trades bandwidth for exactly the CPU the guest needs. That trade is worth it on
a slow link and nowhere else.

This also explains a failure that looked like three unrelated bugs: giving the
guest a 1024x768 screen without updating `index.html` (which hardcoded 800)
broke the `/display` path silently, which left AV1 as the only way to see
anything, which starved the guest to a tenth speed, which made a half-started
desktop look like a dead one.

## Buying CPU share does not buy speed

The app is single-threaded, so it cannot use more than one core whatever
`cpuShare` says. Measured on the fleet, same image and same boot phase:

| cpuShare | vCPU | MIPS | rate |
|---|---|---|---|
| 0.04 | 0.64 | 21.3 | $0.1224/h |
| 0.07 | 1.12 | 21.3 | $0.2124/h |
| 0.25 | 4.00 | 21.8 | $0.7524/h |

Flat across a 6.25x range, including below one core — so on an uncontended node
the share is not a throughput cap. It reads as proportional weight, which only
bites when the node is busy. Buy share for priority under contention, not for
speed; the fleet was ~79% idle for these runs.

## Measuring the interpreter

`/status` reports MIPS, but that figure is not a property of the emulator: the
event loop runs the guest, serves HTTP, scans the framebuffer and encodes video
on one thread, so it moves with what the browser is doing. Optimise against the
benchmark instead, which runs the emulator and nothing else:

```sh
cargo run --release --manifest-path emu/Cargo.toml --example boot-bench -- \
    images/fw_payload.elf images/rootfs.ext2 --until "login:"
```

It reports **instructions** to reach a console marker as well as MIPS, and the
instruction count is the one to watch. It is deterministic — the same guest
doing the same work — so it stays put while the emulator underneath it gets
faster, and it catches the failure that matters most here: a change that speeds
the host loop up while making the *guest* spin for longer is a loss, and only
the instruction count shows it.

That is not hypothetical. Devices used to be serviced on every retired
instruction — six device ticks and two CSR reads wrapped around an instruction
whose own work is a tag compare and an indirect call, which cost more than the
emulation did. They now run every `DEVICE_TICK_INTERVAL` instructions
(`emu/src/cpu.rs`), with device clocks advanced by the whole interval so guest
time passes at the same rate in coarser steps, and with interrupt delivery
re-armed by any CSR write that changes what is pending or enabled. Measured on
the XFCE image, booting to a login prompt:

| interval | instructions | MIPS |
|---|---|---|
| 1 (as before) | 1231M | 29.7 |
| 8 | 1241M | 44.8 |
| **16 (shipping)** | **1233M** | **47.8** |
| 32 | 1229M | 50.8 |

Those are native numbers. **The build that ships is wasm, and it gains less**:
measured under wasmtime against the same image, on the same host, one instance
at a time, sampling instantaneous instret rate rather than `/status` MIPS (that
figure divides by time since start, so the blocking image fetch dilutes it):

| build | MIPS (two runs) |
|---|---|
| before | 17.2, 17.2 |
| after | 23.0, 22.5 |

**1.3x, not 1.6x.** The saving is a fixed cost per instruction, and wasm makes
the *useful* work of an instruction more expensive, so the same saving is a
smaller share of a bigger total. Quote the wasm number when talking about what
a deployment will feel.

Guest work is flat across the whole sweep, so the speedup is real rather than
borrowed from somewhere else. **16 is deliberate, not the fastest number in the
table.** The UART now emits on store (it used to drain one byte per service,
which serialized every console byte behind an interrupt round trip and, at
coarser intervals, dropped characters outright), so output can no longer
corrupt at any interval — but at 64 the guest stops waking on serial *input*
at the shell prompt, so 16 stays.

Setting `DEVICE_TICK_INTERVAL = 1` restores exactly the old behaviour, which is
how the refactor was checked: it reproduces the baseline instruction count to
the digit.

### The 2026-08 rework: superblocks

The table above predates the second round of interpreter work, which replaced
the per-instruction predecode cache with a superblock cache: straight-line
runs of up to 32 pre-decoded instructions execute from a single tag+meta
probe, with operands extracted once at build time and the ~60 hottest integer
and double ops (the FP working set matters: every context switch is a run of
FLD/FSD) inlined behind a jump table instead of an indirect call. Interrupt
delivery, WFI, self-modifying code and precise traps keep their exact
single-step semantics — the boot log is byte-identical, and `bench.py`'s
write-then-execute gate covers the SMC path.

Measured on the sample image, same host, before → after the round:

| | before | after |
|---|---|---|
| native, busy shell workload | 53.2 MIPS | 127 MIPS |
| native, boot to userspace | 1.91 s | 0.88 s |
| wasm (wasmtime), busy | ~63 MIPS | ~85 MIPS |
| native, Alpine desktop first paint | 88 s | ~34 s |

The desktop row got its biggest cut from tagging superblocks by the
PHYSICAL page they were decoded from instead of by translation state:
satp writes and SFENCE.VMA had been invalidating every block on every
context switch, so fault-storm workloads — a desktop assembling itself,
a browser starting — rebuilt the whole cache continuously. The probe
re-translates the start pc through the TLB and compares physical pages,
so remapping can never run a stale block while an unchanged mapping
keeps its blocks across every flush. Measure changes like this with
interleaved A/B runs (alternating builds back to back): a loaded host
makes sequential before/after numbers lie.

An idle (WFI-parked) guest now consumes its tick batch in one step instead of
spinning, so an idle machine costs the event loop ~nothing and the whole
budget goes to the scanout and encoders.

## Caveats, honestly

- **User-mode network, one guest IP (10.0.2.15).** Outbound is NAT at the
  gateway, not a bridge: TCP and UDP flows work, `ping` is answered by the
  gateway itself, and exotic protocols (GRE, SCTP, traceroute's ICMP
  errors...) don't exist. Set `net.outbound: false` for a sealed machine.
- **RISC-V RV64 only, one hart.** RISC Box runs what the vendored emulator runs:
  a single-core RISC-V `virt`-style machine. Not x86, not multi-core.
- **Emulated speed.** Software TLB, a predecoded instruction cache and
  decimated device servicing; fine for a shell, a build, a demo, but not a
  fast VM. There is no KVM in a TEE wasm sandbox, so this is pure
  interpretation and always will be. An idle guest parked in WFI is throttled
  to ~1–2% host CPU; keystrokes force full-speed batches so the console stays
  snappy. See **Measuring the interpreter** below before trying to make it
  quicker — the number the app reports is not the number to optimise against.
- **Blocking image load.** Fetching the images (tens of MB over TLS) happens
  in the event loop, so the console briefly stalls for other clients during a
  boot. One-time, a few seconds.
- **Save consistency.** Save copies the disk while the guest runs; run `sync`
  (or halt) in the guest first, or the snapshot is crash-consistent at best.
  Only the disk is saved, not live RAM, so this is not a suspend/resume.
- **RAM budget.** The machine's memory and both images live in enclave RAM
  (the emulator sizes guest DRAM at 128 MiB); size the deployment accordingly.
- **Credentials.** If the bucket needs credentials and you don't seal them in
  the attested config, they are entered per boot and held only in RAM (a Stop,
  or a restart, drops them).

## Security

The control surface drives a real machine: `/input` and `/exec` are a raw
console into the guest (a root shell, once one is running), and `/start` /
`/stop` / `/save` boot it, halt it, or write its disk back to your bucket. On a
**public** deployment those endpoints are reachable by anyone with the URL, so
set `api_key` (from a `$VAR` secret): it gates `/start`, `/stop`, `/save`,
`/input`, `/exec`, `/console`, and `/status`, leaving only the static shell and
`/ping` open. The browser UI prompts for the key and remembers it for the tab. Without
`api_key`, deploy **private**: an open deployment hands a stranger the
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
bucket's own integrity; a malicious bucket could serve a different kernel, so
treat the bucket as part of your trust base and prefer `https` endpoints.
Confirm the code that runs your machine and handles your credentials with the
deployment's remote attestation at
[enclave.host](https://enclave.host).

The vendored emulator is MIT ([`emu/LICENSE`](emu/LICENSE)); RISC Box itself is
MIT. It is a faithful RISC-V emulator, not a security boundary; the enclave
is the boundary.
