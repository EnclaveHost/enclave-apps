# The `nvenc` verb: host-side spec

> **STATUS: the platform side is built** (EnclaveHost/enclave, `wasm/nvenc-shim/`,
> `wasm/wasmtime-nn-nvenc.patch`, manager gate + `NVIDIA_DRIVER_CAPABILITIES`
> on both GPU flavors). Verified on an RTX 3070: all three settings below are
> in the bitstream, and the three preload paths (encoder present / no GPU /
> toolchain without the backend) behave as specified. What remains is
> operational — dispatch the Wasmtime Toolchain workflow, repin
> `WASMTIME_IMAGE`, redeploy the GPU enclaves — and then section 4, moving this
> app's GameStream host inside the CVM. The spec below is kept as written; the
> one thing it did not anticipate is in section 3.

RISC Box streams its desktop to a real Moonlight client today, but only from a
**native** host process (`gs-bridge/`) running beside the CVM. That works and is
verified end to end, and it is the wrong shape for the product: the sidecar has
to pull plaintext framebuffers out of the enclave and encode them on the host,
so the desktop's pixels, the user's keystrokes and the session keys all live
outside the trust boundary and outside the deployment's attestation. Moving the
GameStream host into the wasm app fixes that, and every piece of it ports except
one.

This app needs ONE addition to the platform (`EnclaveHost/enclave`): a
**video-encode backend** on the wasi-nn surface, in the same shape as the
`sdcpp` backend. Everything else the in-wasm host needs already exists:
inbound `tcp:`/`udp:` listeners in run mode (RISC Box already binds `tcp:2222`),
a public per-deployment IPv6 for raw ports, and ENet/TLS are ordinary portable C
and Rust.

Nothing here asks for tenant kernels — that trilemma is settled. This is a
dedicated interface, the way GPU capability is supposed to grow.

## 0. Why a backend rather than an app-side encoder

Software H.264 in wasm cannot hold 60 fps at desktop resolutions, and the
emulated guest cannot help (no GPU device, RISC-V, no NVENC userspace). The GPU
is reachable only through the wasi-nn shims, and none of them encode video.

## 1. Shim (`wasm/nvenc-shim/enclave_nvenc.{c,h}`, new)

A thin FFI wrapper over NvEncodeAPI, mirroring `enclave_sd`'s role: no cargo
dependencies, linked from `ENV_LIB_LOCATION` at build time.

```c
env_session*  env_open(const env_config* cfg);        /* one encoder session */
int           env_encode(env_session*, const uint8_t* frame, size_t len,
                         int force_idr,
                         uint8_t* out, size_t out_cap, size_t* out_len,
                         int* is_keyframe);
void          env_close(env_session*);
uint32_t      env_caps(void);   /* bit0 h264, bit1 hevc, bit2 av1 */
```

`env_config` carries codec, width, height, fps, bitrate, and input pixel format.

**Three NVENC settings the shim must own, because getting them wrong is silent.**
All three cost real debugging time in `gs-bridge` and none of them are
expressible through the ffmpeg CLI, which is a large part of why this belongs in
a shim rather than a subprocess:

- `repeatSPSPPS = 1`. Moonlight identifies a keyframe by the access unit
  **starting with an SPS**, not by containing an IDR slice
  (moonlight-common-c `VideoDepacketizer.c:isIdrFrameStart`). Emit parameter sets
  once at stream start and every mid-stream IDR is invisible: the client drops
  every frame and times out with `ML_ERROR_NO_VIDEO_FRAME` while the server
  happily reports hundreds of frames sent.
- `enableFillerDataInsertion = 0`. CBR padding is emitted as filler NALs (type
  12) which land *in front of* the SPS. The client skips AUD and SEI when
  looking for that SPS but not filler, so the same total blackout occurs, and
  only against static content — which is exactly what an idle desktop is. On the
  RISC Box console this also inflated keyframes from under 1 KB to 16.9 KB of
  pure padding.
- `NV_ENC_PIC_FLAG_FORCEIDR` honoured per frame. Moonlight requests an IDR on
  packet loss; a host that cannot produce one on demand recovers only at the next
  GOP boundary. `gs-bridge` cannot do this at all today (it drives ffmpeg through
  a pipe and can only shorten the GOP), so the verb is strictly better than the
  native implementation it replaces.

## 2. Backend (`wasm/wasmtime-nn-nvenc.patch`, new)

Follows `wasmtime-nn-sdcpp.patch` exactly:

- **Cargo feature** `nvenc` in `crates/wasi-nn/Cargo.toml`, alongside `ggml`,
  `onnx`, `sdcpp` (sdcpp patch, `Cargo.toml` hunk).
- **build.rs**: link `enclave_nvenc` from `ENV_LIB_LOCATION`, the same three
  lines sdcpp uses for `ESD_LIB_LOCATION`.
- **`backend/mod.rs`**: `pub mod nvenc;` + `NvencBackend` in `list()`.
- **Preload-only**, like sdcpp: `-S nn-graph=nvenc::<dir>` + `load_by_name`.
  There is no model file; the graph is the encoder itself.

**The one structural difference from every existing backend: encode is
stateful.** Frame N references frame N-1, so a session cannot be reconstructed
per call. Map it onto the API that already exists for this:

- `init_execution_context(graph)` opens **one NVENC session** (`env_open`).
- dropping the context closes it (`env_close`).
- a guest that wants two streams inits two contexts.

That keeps the whole thing inside the current wasi-nn shape rather than
inventing a session handle in the tensor namespace.

### Verbs

| input | type | meaning |
|---|---|---|
| `config` | U8 [n] | JSON: codec, width, height, fps, bitrate. Once per context, before the first frame; a second `config` reconfigures. |
| `frame` | U8 [n] | one raw frame, NV12 by default |
| `idr` | I32 [1] | optional, beside `frame`: force this frame to be an IDR |
| `caps` | I32 [1] | probe, no session needed |

| output | type | meaning |
|---|---|---|
| `bitstream` | U8 [n] | Annex-B for exactly one access unit |
| `keyframe` | I32 [1] | 1 if the returned unit is an IDR |

`caps` mirrors the ggml probe: guests read missing slots as "no", so an older
host degrades honestly and the app falls back to reporting that streaming is
unavailable rather than trapping.

**Prefer NV12 over RGB.** A frame crosses the sandbox boundary on every call:
RGB24 at 1080p is 6.2 MB, NV12 is 3.1 MB. At 60 fps that is the difference
between ~370 MB/s and ~185 MB/s of copy. Worth confirming against
`hostcall-fuel` (default 128 MiB per call, already raised to 4 GiB for nn
tenants in `wasm_manager.py`) — per-call size is fine either way, but the
sustained rate is the thing to measure.

## 3. Manager (`wasm/wasm_manager.py`)

- **Not in the original spec, and it is the thing that would have made this
  fail on a working card:** the NVIDIA container runtime injects driver
  libraries by *capability*, and the default `compute,utility` does not include
  `libnvidia-encode.so.1`. Without `NVIDIA_DRIVER_CAPABILITIES` carrying
  `video`, the H200 is present, CUDA initializes, and the encoder dlopen finds
  nothing — which reads as "this card cannot encode". Set on the `wasm-manager`
  container (the one that spawns tenant wasmtimes) in both `enclaves/gpu` and
  `enclaves/gpu8`.
- Rides the existing `-Snn` grant and `gpuShare` purchase; no new launch flag.
- **Accounting needs thought, and it is not the SM share.** The arbiter
  (`wasmtime-nn-arbiter.patch`) exists because MPS statically partitions SMs.
  NVENC is a *separate fixed-function engine* — an encode turn consumes almost
  no SM time (measured 4-5% encoder utilization for one 720p stream on an RTX
  3070, against 8-9% overall). Wrapping `env_encode` in `arbiter::turn()` would
  queue encodes behind inference tenants that are not competing for the same
  silicon. My read is that encode wants its own arbiter class, or none at all
  with a per-deployment session cap instead. This is the open design question in
  this spec and it is yours, not mine.
- **Concurrent session limits are a fleet capacity question.** NVENC caps
  simultaneous sessions per card; datacenter parts are unrestricted where
  consumer parts are not, so the H200 is the easy case, but the cap needs to be
  known before selling N streaming deployments per node.

## 4. What it unblocks

With this verb, the whole GameStream host moves inside the CVM: pairing, the
HTTPS control surface, RTSP, RTP with Reed-Solomon FEC, and the encrypted
control channel are all already written and tested in `gs-bridge/` as portable
Rust, and the framebuffer stops making an HTTP round trip out of the enclave
because the emulator and the streaming host become the same module. Pixels,
keystrokes and session keys stay inside the boundary and under attestation, with
the residual exposure being the GPU itself — the same one every GPU tenant
already accepts today.

It is also not RISC-Box-specific. Any app that can produce frames gets remote
desktop or video streaming out of it.

## 5. Not in scope here

- **Video-stream encryption.** `gs-bridge` currently negotiates
  `SS_ENC_CONTROL_V2` only, so input is encrypted but video and audio are in the
  clear. That is an app-side fix (the client supports `SS_ENC_VIDEO`; the format
  is a 12-byte IV plus GCM tag prefixed per shard, keyed off the launch `rikey`)
  and it needs doing before anything is exposed on a public address, whichever
  way the encode lands.
- **Audio.** The emulated machine has no sound device; real audio needs a
  virtio-sound device in `emu/`, not platform work.
- **Decode.** Nothing here needs NVDEC.
