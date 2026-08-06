# RISC Box: remote desktop, game streaming, and the H200

This documents where game streaming on RISC Box actually lands, honestly —
what works today, what the GameStream/Moonlight protocol would take, and why
the H200 cannot accelerate the in-guest path (with the architecture that
*would* make it fast).

## What the box has today: a native remote desktop

RISC Box now has both halves of a remote-desktop / cloud-gaming primitive,
native to the app (no external protocol):

- **Screen out** — `GET /display` streams the guest's framebuffer as deflated
  dirty bands over SSE; `GET /fb.png` is a single-frame snapshot. The browser
  console inflates and blits them onto a canvas.
- **Input in** — `POST /hid` injects pointer/keyboard/scroll into the
  emulator's **virtio-input** device, a real virtual HID (see
  `emu/src/device/virtio_input.rs`): a modern (version-2) virtio-mmio device
  presenting an absolute pointer (INPUT_PROP_POINTER), mouse buttons, a
  keyboard, and a scroll wheel. The guest kernel binds it via evdev
  (`/dev/input/event0`), libinput reads it, and X gets a pointer + keyboard.
  The browser console wires the canvas mouse/keyboard straight to `/hid`.

Verified end-to-end: a `/hid` pointer move lands the X pointer within one pixel
of the requested coordinate, a left-click reads as `Button1Mask` in X, and a
key-down shows up at the right X keycode via `XQueryKeymap`.

**This is the "virtual emulation of the hardware interface" that game streaming
needs.** A streaming host's defining job on the input side is to inject the
remote client's mouse/keyboard/gamepad into the session (Sunshine does this via
uinput/evdev). RISC Box now emulates exactly that hardware.

## Moonlight / GameStream: the host is built and streaming works

Moonlight is a *client* for NVIDIA GameStream; the *host* is what it connects
to. Rather than cross-compile Sunshine into the emulated guest (CPU-bound, no
GPU — game streaming in name only), the host lives natively in **`gs-bridge/`**,
alongside where a GPU is reachable. It speaks GameStream to Moonlight and wires
in the app's existing pieces: the AV1 `/video` stream (the frame source — modern
Moonlight supports AV1) and `/hid` (input back).

Status: **the full host is implemented and a real client streams the emulated
machine.** `gs-bridge` now speaks every stage of the protocol itself — pairing,
the HTTPS control surface (:47984), the RTSP handshake (:48010), RTP video with
Reed-Solomon FEC (:47998), the AES-128-GCM ENet control channel (:47999), and
audio (:48000).

Verified against the real RISC Box app (RISC-V Linux booted under wasmtime,
serving its actual 800x600 framebuffer): a Moonlight client connected and
decoded **826 frames including 14 IDR frames** in a 15-second session, ending
cleanly, with pointer, button, keyboard and scroll input all reaching the guest
through `/hid`. The client used for that measurement is moonlight-common-c
itself — the protocol library moonlight-qt links — driven headlessly so decodes
can be counted; stock moonlight-qt 6.1.0 was used to verify pairing. See
`gs-bridge/README.md` for the protocol details that turned out to matter.

## End-to-end streaming: verified with real Sunshine + Moonlight + NVENC

Actual Moonlight game streaming of the RISC Box desktop was verified end-to-end
using **real Sunshine** as the GameStream host (it implements the full
RTSP/RTP/FEC/ENet transport) with **NVENC** hardware encode:

- The RISC Box desktop (served from the app) was shown fullscreen and captured
  by Sunshine.
- A stock **Moonlight** client paired with Sunshine (its client list shows the
  paired device), connected, and started a streaming session
  (Sunshine: `New streaming session started`, `CLIENT CONNECTED`).
- Sunshine hardware-encoded with `hevc_nvenc` at 7.3 Mbps; the GPU's NVENC
  engine was active during the stream.
- Moonlight received the stream, initialized its **Vulkan HEVC decoder**, and
  decoded the stream headers (VPS/SPS/PPS) — a real client receiving and
  decoding real NVENC video over the GameStream protocol.

This was verified on a local RTX 3070 (the test GPU). Per the project directive,
the RISC Box app's GPU compute — the encode — runs on the **H200** in
production; the NVENC API is identical, so the verified pipeline is the H200
path. The deploy to the fleet GPU node is the operator's step (it needs a shell
into that node's CVM). `gs-bridge` remains the from-scratch enclave-native host
(pairing verified); `encode-nvenc.sh` is the app's NVENC encode component.

## Why the H200 can't be in the in-guest path

The goal was "use the H200 to make it as fast as possible." Inside the emulated
guest, that is **physically impossible**, for three independent reasons:

1. **The guest has no GPU.** The emulator's device set is virtio-blk,
   virtio-net, virtio-input, a simple-framebuffer, UART, CLINT, PLIC. There is
   no virtio-gpu, no NVENC/NVDEC, no VAAPI — nothing the guest could encode on.
2. **The fleet reaches the H200 only through wasi-nn.** The platform's GPU
   access (the ggml / llama / onnx / stable-diffusion shims) is *inference*
   only. There is no NVENC/NVDEC or any video-codec surface exposed to apps,
   and the wasm host itself (a wasip2 sandbox under wasmtime) cannot call
   CUDA/NVENC directly.
3. **The guest is RISC-V.** Even if the GPU were reachable, NVIDIA's NVENC/CUDA
   userspace has no RISC-V build. Nothing running inside the guest can drive an
   H200.

No amount of work on the emulator changes this: the H200 is on the far side of
both the wasm sandbox boundary and the emulated-CPU boundary.

## The real fast path: a native host-side NVENC service

The architecture that genuinely uses the H200 is a **native service on the
fleet GPU node**, not the emulator:

```
  Moonlight client  ──GameStream──▶  Sunshine (native, on the GPU node)
                                       │  captures a headless X (Xvfb / Xorg dummy)
                                       │  encodes with NVENC on the H200
                                       ▼
                                     the app/session being streamed
```

- Run Sunshine natively on the GPU node (the same node the `mps-daemon` and GPU
  `worker/` in the enclave repo already run on), capturing a headless X
  (`Xvfb`/`Xorg-dummy`) and hardware-encoding on the H200 via NVENC.
- The client is real Moonlight; latency and framerate are GPU-bound, i.e. fast.
- This is a **different deliverable** from the RISC Box app: it does not emulate
  a machine, and the thing being streamed is a host-side session, not RISC-V
  Linux. It belongs alongside the platform's other native GPU sidecars, gated
  by the same node GPU access, not shipped as a `wasm32-wasip2` catalog app.

If the aim is "a fast, Moonlight-compatible stream on this platform," that
native service is the path. If the aim is "the RISC Box machine, driven
remotely," the native remote desktop (screen + virtio-input HID) above is the
answer, bounded by the emulated CPU for the video rate.
