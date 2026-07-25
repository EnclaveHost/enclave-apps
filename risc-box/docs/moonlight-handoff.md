# Handoff prompt — RISC Box Moonlight game streaming

Paste everything below to the next LLM. It is self-contained.

---

You are picking up work on **RISC Box**, a `wasm32-wasip2` catalog app in the
Enclave platform monorepo layout:

- App source: `~/Projects/enclave-apps/risc-box/`
- Platform/relay/supervisor: `~/Projects/enclave/`

RISC Box is a pure-Rust RISC-V (RV64GC, Sv39) emulator compiled to
`wasm32-wasip2` and run under wasmtime (~25 MIPS). It boots Linux with an Xorg
desktop (a spinning-cube demo) and exposes an HTTP API. It runs on the Enclave
fleet; the GPU node is an **NVIDIA H200** reached via the platform's wasi-nn
shims.

## The goal (unchanged, verbatim)

> go further and install/configure moonlight game streaming. Build a virtual
> emulation of any hardware interface it needs to work. Try to use the H200 to
> make it as fast as possible.

Two hard constraints the operator (Steven) added mid-flight:

1. **The RISC Box app's GPU compute MUST run on the H200 in production.** A local
   RTX 3070 is fine *for testing only*; the production video encode is the H200.
2. Standing repo rule: **commit and push to `main` after any change, unprompted.**
   Commit trailer: `Co-Authored-By: Claude <noreply@anthropic.com>`.

## Mental model — where the H200 actually fits (read this first)

"Use the H200 to make it fast" has one physically correct meaning: the **video
encode** is the GPU compute, and it runs on the GPU's **NVENC** hardware engine —
*not* inside the emulated guest. Three independent walls make in-guest GPU use
impossible, and you should not try to knock them down:

1. The emulated guest has no GPU device (virtio-blk/net/input + simple-fb + UART
   + CLINT + PLIC only — no virtio-gpu, no NVENC).
2. The fleet reaches the H200 only through wasi-nn (inference shims); there is no
   NVENC/NVDEC surface exposed to apps, and the wasip2 sandbox can't call CUDA.
3. The guest is RISC-V; NVIDIA's NVENC/CUDA userspace has no RISC-V build.

So the architecture is: the wasm app produces frames; a **native host-side
service on the GPU node** pulls those frames and hardware-encodes them on the
H200's NVENC engine; a real Moonlight client receives them. This is documented in
`risc-box/docs/streaming.md` — trust it.

## What is DONE and VERIFIED

**1. Virtual hardware for input — the HID the streaming needs.**
`emu/src/device/virtio_input.rs` is a modern (version-2) virtio-mmio virtio-input
device at MMIO `0x10003000`, IRQ 3. It presents an absolute pointer
(INPUT_PROP_POINTER, ABS_X/ABS_Y 0..32767), mouse buttons, a keyboard, and a
scroll wheel. Guest binds it via evdev (`/dev/input/event0`); libinput reads it;
X gets a real pointer+keyboard. `POST /hid` injects move/moveabs/button/key/
scroll → EV_SYN. Verified: a `/hid` move lands the X pointer within one pixel; a
click reads as `Button1Mask`; a key-down shows at the right X keycode.
CRITICAL gotcha already solved: virtio-input is **modern-only** — the guest's
`virtio_input.c` requires `VIRTIO_F_VERSION_1`; a legacy (version-1) device
probes but silently FAILS (status 0x83). Guest build also needed **eudev**
(`BR2_ROOTFS_DEVICE_CREATION_DYNAMIC_EUDEV=y`) so libinput/evdev X drivers build.
(Memory: `risc-box-virtio-input-hid.md`.)

**2. Frame source for the encoder.** `GET /fb.rgb` in `src/main.rs` serves the
raw 800×600 RGB framebuffer — the input the native NVENC encoder consumes. (The
app also has `GET /display` SSE + `GET /fb.png` for the browser console, and a
software AV1 path `GET /video` via rav1e for a browser-grade client.)

**3. GameStream pairing — from scratch, verified against a real client.**
`gs-bridge/` is a native Rust GameStream host (openssl). It implements
`/serverinfo` (discovery) and `/pair` (the full 4-phase AES-128-ECB / SHA-256 /
RSA handshake), crypto mirroring Sunshine's `nvhttp.cpp` + `crypto.cpp` exactly.
Verified: stock **moonlight-qt 6.1.0** discovered "RISC Box", ran all four phases,
both crypto checks passed (`hash_ok=true sig_ok=true → *** PAIRED ***`), advanced
to `/applist`, host reports `PairStatus=1`. GameStream pairing is plain HTTP
:47989 (no TLS). See `gs-bridge/README.md`.

**4. NVENC GPU encode — verified.** `gs-bridge/encode-nvenc.sh` pulls `/fb.rgb`
and hardware-encodes via ffmpeg `h264_nvenc`/`hevc_nvenc`/`av1_nvenc`. Verified on
the RTX 3070: encoding the desktop drove the GPU's **NVENC engine to 100%
utilization**, producing valid yuv420p H.264 that decodes back to the desktop.
`h264_nvenc` errors out if NVENC is absent (never CPU-fallback), so a successful
run is itself proof the GPU did it. Same NVENC API on 3070 and H200.

**5. End-to-end streaming — verified with real Sunshine + Moonlight + NVENC.**
Because gs-bridge's own RTSP/RTP transport isn't built yet, the full end-to-end
path was proven with **real Sunshine** (AppImage, non-root) as the GameStream
host: RISC Box desktop shown fullscreen, Sunshine X11-captured it and encoded with
`hevc_nvenc` @7.3 Mbps (NVENC engine active); a stock **Moonlight** client paired
(client list shows the device), connected (`New streaming session started`,
`CLIENT CONNECTED`), and received + **Vulkan-HEVC-decoded** the stream headers
(VPS/SPS/PPS). On the 3070 (test GPU); production encode = H200.
GOTCHA: the moonlight and sunshine AppImages both `--appimage-extract` to the same
`squashfs-root/` and COLLIDE — run each via `./X.AppImage
--appimage-extract-and-run` (private temp dir).

(Memory: `risc-box-moonlight-streaming.md`, `risc-box-xorg-guest.md`.)

## What is LEFT

**A. Production H200 deploy — the operator's step (blocked for an LLM).** The
native encode/host service must run on the fleet GPU node, co-located with the
RISC Box CVM. That needs a shell into that node's Tinfoil-managed CVM, which the
dev box cannot reach — Steven explicitly owns this step. If you're an LLM without
fleet GPU-node access, you CANNOT do this; do not fake it. The local 3070
verification IS the H200 path (identical NVENC API); the remaining work is
operational placement, not code.

**B. gs-bridge's own from-scratch transport (the enclave-native host).** To
replace real-Sunshine with gs-bridge as the host, gs-bridge still needs the
post-pairing protocol (this is the large multi-session piece; pairing + codec +
input are already done):
- **HTTPS :47984** — post-pair control (`/applist`, `/launch`, `/resume`) using
  the paired cert. Moonlight moves here immediately after pairing.
- **RTSP :48010** — the stream-negotiation handshake.
- **RTP video :47998** — packetize the NVENC (or AV1) frames in Moonlight's video
  packet format with **Reed-Solomon FEC**.
- **ENet control :47999 (AES-GCM)** — input + keepalives; map input events to the
  app's `POST /hid`.
- **Audio :48000** — Opus over RTP (optional).
Reference implementation to mirror: Sunshine (`src/rtsp.cpp`, `src/stream.cpp`,
`src/nvhttp.cpp`). Deploy/verify on the fleet H200 node (needs A's access).

## How to work / reproduce locally

- Build the app: `cd risc-box && cargo build --release --target wasm32-wasip2`
  (guest image build is heavier — see `guest/`; the prebuilt guest is committed).
- Run the app locally under wasmtime; it serves on a local port (the test rig
  used `http://127.0.0.1:18010`). `GET /fb.rgb` is the encoder's frame source.
- Build the bridge: `cd gs-bridge && cargo build --release`, run
  `./target/release/gs-bridge` (host on :47989).
- Pairing test: pre-seed the PIN
  `curl 'http://127.0.0.1:47989/pin?uniqueid=0123456789ABCDEF&pin=1234'` then
  `moonlight pair <host-ip> --pin 1234` → `*** PAIRED ***`.
- NVENC proof: `gs-bridge/encode-nvenc.sh http://127.0.0.1:18010 hevc_nvenc out.mp4`
  while watching `nvidia-smi --query-gpu=utilization.encoder --format=csv,noheader`.
- Recurring shell gotcha: `pkill`/`pgrep` return nonzero and abort a compound
  bash line — run them alone or `for p in $(pgrep X); do kill $p; done`.

## Honesty rules for this task

- The local GPU is a 3070; production is the H200 — say so, don't conflate them.
- "Pairing verified" and "end-to-end verified via real Sunshine" are true;
  "gs-bridge streams video by itself" is NOT yet true (transport unbuilt).
- Don't claim the H200 was used unless you actually deployed to and ran on the
  fleet GPU node.
