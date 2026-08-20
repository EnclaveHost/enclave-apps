# Handoff prompt: build the RISC Box → H200 NVENC video encode path

You are picking up an autonomous build on the **RISC Box** app
(`~/Projects/enclave-apps/risc-box`, part of the public `EnclaveHost/enclave-apps`
repo; the platform repo is `~/Projects/enclave/`). RISC Box is a
`wasm32-wasip2` service that runs a vendored pure-Rust RISC-V system emulator
booting Linux from S3, with the serial console + framebuffer bridged to the
browser. It runs under wasmtime inside a confidential VM (TEE) on the fleet's
GPU node, which has an NVIDIA **H200**.

Read this whole doc before touching anything. Also read `docs/streaming.md`
(the architecture rationale) and the memory notes `[[risc-box-virtio-input-hid]]`,
`[[risc-box-xorg-guest]]`, `[[nan-wasi-nn-gpu-interface]]`, `[[enclave-apps-repo]]`,
`[[nan-enclave-cli]]`, `[[commit-push-after-changes]]`.

## The goal

Stream the RISC Box guest desktop to a client as real, efficiently-encoded
video, **hardware-accelerated on the H200**, as fast as possible. Originating
directive: "install/configure moonlight game streaming, build a virtual
emulation of any hardware interface it needs, use the H200 to make it as fast
as possible." Moonlight-protocol compatibility is desirable but secondary to a
working, H200-accelerated encode of the desktop.

## The one architectural decision that makes this tractable

**Encode in the app, not in the guest.** The emulated RISC-V CPU runs at
~20–30 MIPS, so any encoder *inside the guest* (Sunshine/x264) runs at well
under 1 fps — and there is no RISC-V NVIDIA driver/NVENC userland, so the guest
can never drive the GPU directly. But the RISC Box **app** — the
`wasm32-wasip2` host program that contains the emulator — runs under wasmtime's
JIT at near-native speed and has direct native-speed access to the guest
framebuffer (it is host RAM; `Emulator::read_physical_range` is a slice copy).
So capture + encode + serve-the-stream all belong in the app. The guest only
runs the desktop and receives input.

This sidesteps the entire "cross-compile Sunshine for RISC-V" problem. The
"Sunshine role" (capture → encode → serve a streaming protocol) moves into the
app, where it is fast and where the GPU is reachable.

## What is already built and verified (committed on `main`)

1. **Virtual input hardware** (`[[risc-box-virtio-input-hid]]`) — a modern
   virtio-mmio **virtio-input** device in the emulator (0x10003000, IRQ 3;
   `emu/src/device/virtio_input.rs`) + `POST /hid` + browser wiring. Verified
   end-to-end: `/hid` pointer moves land within 1px, buttons read as
   Button1Mask, keys map to the right X keycodes. This is the input half of
   streaming — done. (Gotcha learned: virtio-input is MODERN-only, needs
   VIRTIO_F_VERSION_1; a legacy device probes but fails silently. eudev is
   required in the guest for libinput to build.)

2. **The encode seam + first backend** (`src/video.rs`, this session):
   - `trait VideoEncoder { fn encode(&mut self, rgb, w, h) -> EncodedFrame; fn mime(); }`
     — the swappable backend interface.
   - `capture_rgb(emu) -> (rgb, w, h)` — the shared capture front-end (native
     memcpy out of guest FB at 0x87e00000, BGRX→RGB). This is the only
     per-frame work; it is NOT on the emulated CPU.
   - `MjpegEncoder` — pure-Rust Motion JPEG (jpeg-encoder crate), the first
     real codec on the path. Intra-only/bandwidth-heavy but proves
     capture→encode and runs in milliseconds app-side.
   - `GET /frame.jpg?q=<1..100>` in `src/main.rs` — one encoded frame. (Should
     be tested against the live guest; if you're reading this before that was
     confirmed, verify: fetch it, check it's a valid JPEG that decodes to the
     desktop and changes frame-to-frame.)
   The existing `/display` (deflate-band SSE) and `/fb.png` remain; `/frame.jpg`
   is the video-codec path's first rung.

## The plan — three encoder tiers, cheapest to fastest

- **Tier 1 (done): Motion JPEG in the app.** Works with no platform changes.
  Bandwidth-heavy (no inter-frame), but a real, streamable codec. Good enough
  to stand up the transport and prove the whole pipeline.
- **Tier 2: software H.264/HEVC in the app.** Compile openh264 (or x264, or a
  Rust encoder) into the wasm and add an `H264Encoder: VideoEncoder`. Inter-frame,
  ~10–50x smaller bitstream than MJPEG. Runs app-side (wasm-JIT), so plausibly
  real-time at 800×600. This is the fallback if the H200 turns out unreachable.
- **Tier 3: NVENC on the H200 (the goal).** Offload encode to the GPU's video
  engine; the app's per-frame cost drops to just the capture memcpy. Details
  below.

## Tier 3: the H200 NVENC path — how to actually build it

### Step 0 (GATING, do first): is NVENC reachable in the CVM?

Everything downstream depends on this and it is currently UNVERIFIED. The GPU
is already live in the confidential VM for CUDA inference (the wasmtime
toolchain injects `libcuda.so.1` and runs ggml/onnx-CUDA — see
`~/Projects/enclave/wasm/Dockerfile.wasmtime` and the shims in `wasm/llama-shim/`,
`wasm/sd-shim/`). NVENC is a *separate engine* on the same card, driven by
`libnvidia-encode` from the same driver package. Two unknowns:
  - Is `libnvidia-encode` present in the CVM, and does an NVENC encode session
    actually initialize under the **H200's confidential-compute passthrough
    mode**? CC mode is validated for the compute engines; the video engines'
    availability under CC + the fleet's MPS partitioning is not something the
    prior session could confirm (no root shell into the Tinfoil-managed CVM).
  - Does MPS (which partitions compute contexts) even gate NVENC, or is the
    video engine shared/free?
How to check: get visibility into the GPU node's CVM (ask the operator/Steven
for access, or add a one-shot probe to the toolchain image that tries
`nvEncOpenEncodeSessionEx` and logs the result). If NVENC is blocked under CC,
stop here and ship Tier 2 (software H.264) — do not fake H200 acceleration.

### The implementation shape (once NVENC is confirmed)

The encode must run in **host-side native code inside the CVM** (the wasm app
can't call NVENC directly; only the host can). Two options — pick one:

**Option A — extend the wasi-nn-style host surface (cleanest long-term).**
Add a host module to the custom wasmtime build (same image that carries the
wasi-nn ggml/onnx patches, `~/Projects/enclave/wasm/`) exposing an encode API to
the guest wasm via a WIT interface: `open(w,h,codec,bitrate) -> session`,
`encode(session, frame_ptr, len) -> packet`, `close(session)`. Host side links
`libnvidia-encode` and runs NVENC on the H200. The RISC Box app imports this
interface and its `NvencEncoder: VideoEncoder` calls it. This touches: the
wasmtime Dockerfile/features, a new host module (C or Rust linking the NVENC
SDK), the WIT, and the wasm-manager plumbing (like `-S nn`, gated by gpuShare).
Rides the toolchain-image release/repin cycle (note: other toolchain changes
are "INERT until WASMTIME_IMAGE repin").

**Option B — a native GPU sidecar in the CVM (fewer platform changes).**
Run a small native process on the GPU node (like the ggml backend is native
host code) that does NVENC; the RISC Box app ships raw frames to it over a
local socket / loopback and gets packets back. The app already captures frames
and already has socket access. Frames stay inside the CVM/TEE. This avoids
patching wasmtime's host interface but adds a sidecar to deploy/attest.

Either way, the app change is small and localized: a new `NvencEncoder`
implementing `VideoEncoder`, selected by config. The capture front-end and the
transport are unchanged.

### The transport (client-facing)

`/frame.jpg` is a single frame; a stream needs a transport. Options, by
client:
- **Browser, simplest:** Motion-JPEG-over-SSE or a `multipart/x-mixed-replace`
  endpoint (needs a new long-lived binary response type in `src/httpd.rs`, which
  today only does one-shot responses + text/SSE). Good for Tier 1.
- **Browser, H.264:** fragmented-MP4 or Annex-B over a WebSocket into
  MediaSource / WebCodecs. Best quality path for a browser client.
- **Moonlight-compatible:** implement the GameStream control/RTSP + RTP video
  path in the app (this is the "Moonlight game streaming" literal goal). Large,
  but now feasible because encode is fast (app-side/NVENC). This is where
  Moonlight-protocol work lives, if wanted — reuse the encoded packets from the
  `VideoEncoder`.
Recommend: build the browser H.264-over-WebSocket transport first (proves the
end-to-end encoded stream with a real client), then GameStream if Moonlight
compatibility is required.

## Exact next steps (in order)

1. Confirm `/frame.jpg` works against the live guest (may already be done).
2. Stand up a **stream transport** for Tier 1: a Motion-JPEG stream the browser
   plays (SSE-JPEG is least invasive; `multipart/x-mixed-replace` needs an
   httpd streaming-response type). Wire it into `src/index.html`. This proves
   capture→encode→**stream** end-to-end with a client.
3. **Tier 2**: add a software H.264 backend (openh264 compiled to wasm, or a
   Rust encoder) as `H264Encoder: VideoEncoder`; add an H.264 transport
   (WebSocket + MediaSource/WebCodecs). Now you have efficient encoded video
   with no GPU dependency — the shippable fallback.
4. **Step 0 gating check** for NVENC-under-CC (can be done in parallel; it
   decides whether Tier 3 is possible).
5. **Tier 3**: implement the host encode path (Option A or B) and
   `NvencEncoder`, select it by config, measure fps/latency vs Tier 2.
6. Ship: publish/deploy to the fleet (the `[[risc-box-xorg-guest]]` recipe;
   dev-mode private deploy first), verify the H200-encoded stream in the TEE.
7. Optional: GameStream/RTP transport for literal Moonlight compatibility.

## Test rig, paths, gotchas

- **Local guest:** minio on 127.0.0.1:9100 (minioadmin/minioadmin, bucket
  `machines`, images under `xorg/`). Build tooling from the prior session:
  `PRIOR=/tmp/claude-1000/-home-steven-Projects-enclave/2788dcc3-ffa3-48cc-9fbb-5bc32513b07e/scratchpad`
  (Buildroot at `$PRIOR/buildroot`, out-of-tree build `$PRIOR/br-build`, images
  `$PRIOR/br-build/images/{fw_payload.elf,rootfs.ext2}`, cross gcc
  `$PRIOR/br-build/host/bin/riscv64-linux-gcc`).
- **Rebuild app wasm:** `cd ~/Projects/enclave-apps/risc-box && cargo build
  --release --target wasm32-wasip2`.
- **Boot locally** (as its own `run_in_background` bash, `exec wasmtime …`;
  reseed minio first with `scripts/seed-machine.py put ... xorg/fw_payload.elf`
  and `xorg/rootfs.ext2`):
  `wasmtime run -Stcp -Sudp -Sinherit-network -Sallow-ip-name-lookup --env
  ENCLAVE_PORTS="http:8000=18010,tcp:2222=12222" --env RISCBOX_CONFIG='{"title":"xorg-local",
  "endpoint":"http://127.0.0.1:9100","region":"us-east-1","bucket":"machines",
  "kernel":"xorg/fw_payload.elf","fs":"xorg/rootfs.ext2","saveKey":"xorg/rootfs.saved.ext2",
  "autostart":true,"credentials":{"accessKeyId":"minioadmin","secretAccessKey":"minioadmin"},
  "net":{"forwards":[{"listen":2222,"to":22}]}}' ~/Projects/enclave-apps/risc-box/target/wasm32-wasip2/release/risc-box.wasm`
  → HTTP on 127.0.0.1:18010, SSH forward 12222. X boot is ~2–5 min; poll
  `/fb.png` or `/frame.jpg`. SSH: `ssh -p 12222 -i
  ~/Projects/enclave-apps/risc-box/images/riscbox_ed25519 -o StrictHostKeyChecking=no
  -o UserKnownHostsFile=/dev/null root@127.0.0.1` (filter openssh PQ warnings).
- **Test /frame.jpg:** `curl -s http://127.0.0.1:18010/frame.jpg -o f.jpg`;
  verify it decodes (Python `PIL`/`imghdr`, or the JPEG SOI/EOI markers
  FFD8..FFD9) and that two grabs a few seconds apart differ (the cube spins).
- **Fleet publish/deploy:** ask the operator for the wallet key and the R2
  `machines` credentials, and keep both out of the tree. The key that used to
  sit here (addr 0x337EcabC…7319) and the R2 access/secret pair beside it were
  published with this file and must be treated as burned — see the sibling
  `engine-freeze-handoff.md`, where the same habit cost a wallet: a leaked key
  there was picked up within a day, delegated via EIP-7702 and swept. R2
  endpoint (not a secret):
  `https://0f4fd20d9b44134b04692dd8b6f50e30.r2.cloudflarestorage.com`. NEVER put
  S3 keys in on-chain config — use `$S3_*` placeholders + `deploy --secrets`.
  Full recipe (publish → dev-mode private deploy → SOCKS-egress + boot-retry
  gotchas → owner-token verify) is in `[[risc-box-xorg-guest]]`.
- **Gotchas:** ext2 not ext4; OpenSBI v0.9 non-PIC; DRAM size must match
  `emu/src/lib.rs` AND the DTB; dtb.dtb is `include_bytes!`-embedded (recompile
  with dtc after editing dtb.dts). `pkill`/`pgrep` return nonzero and abort a
  compound bash line — run them alone or `|| true`. Standing rule: commit +
  push to main after any repo change.
- **Honesty rule (important):** the goal's "H200 fast" is gated on Step 0. If
  NVENC is not reachable under CC mode, the honest outcome is Tier 2 (software
  H.264, app-side) shipped + the boundary documented — NOT a claim of H200
  acceleration. Report what actually runs.
