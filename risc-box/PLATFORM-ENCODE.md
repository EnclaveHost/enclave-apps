# PLATFORM-ENCODE: a video-encode verb, so a GPU share can carry the stream

The ask: a platform verb that takes raw frames and returns H.264/HEVC/AV1
access units, backed by NVENC on the fleet GPU, metered against the
deployment's gpuShare like every other verb. This is the ONLY
platform-legal way "offload compute to GPU" applies to the RISC Box: the
no-tenant-kernels decision is settled, wasi:nn-style verbs are how GPU
capability grows, and encode is a fixed-function unit — no tenant-authored
code reaches the GPU at all.

## Why (measured)

- kryptos, minih264 in-wasm: **~23 ms per 1024x768 frame** — a hard ~43 fps
  encode ceiling on a machine we now want streaming at 60.
- The encode already runs on a SET worker, so this does NOT buy guest MIPS —
  it buys the STREAM: 60 fps H.264 with the CPU share untouched, and it is
  the only consumer for a DOOM-box gpuShare (0.01 is plenty; NVENC is a
  dedicated ASIC block, near-zero SM usage — worth metering by session, not
  TFLOPs).
- Bands/raw alternatives ship 32x the egress of in-enclave H.264 and leak
  the raw framebuffer out of the TEE (measured 2026-08-20). Encode must stay
  inside; this verb keeps it inside AND fast.

## Shape (mirrors the wasi:nn/image-verb conventions)

    encoder.open(codec: h264|hevc|av1, w, h, kbps, gop, profile) -> handle
      // rejects when no gpuShare or no NVENC session available
    encoder.frame(handle, bgrx_bytes, force_idr: bool) -> au_bytes
      // synchronous; BGRX in (the app's capture format), one AU out.
      // Contract matches the app's VideoEncoder trait (src/video.rs):
      // gop=0 = IDR only on demand; rate control must hold ~kbps with a
      // <= half-second VBV (the minih264 settings that survived live use:
      // fine_rate_control, intra qp floor ~26, 4x P budget — see
      // risc-box-moonlight memory for why each exists).
    encoder.close(handle)

- Frame size cap 4 MiB (1024x768x4 fits); per-deployment session cap 1-2.
- Failure mode: verb absent or open() refused -> the app KEEPS its minih264
  worker path. The app change is ~30 lines: a VideoEncoder impl that
  prefers the verb and falls back — the trait was built for exactly this
  swap ("a backend swap, not a rewrite").
- Privacy note for the docs: frames leave the wasm sandbox but not the CVM;
  NVENC runs inside the same confidential boundary that already runs
  wasi:nn inference on tenant data.

## Metering

Bill like inference verbs: per open session + per encoded megapixel-second
against gpuShare. A 1024x768@60 stream is ~47 Mpx/s — price so that
gpuShare 0.01 covers one such session with headroom.

## Order of work

1. Supervisor: NVENC session pool + the three verbs, capability-gated
   (caps entry like the image verb; deployments opt in via gpuShare > 0).
2. Toolchain: import surface for the verbs in the app sysroot (same
   plumbing as the image verb got in 0.19).
3. App: the VideoEncoder impl + `encoder:"gpu"` config knob, fallback
   default on.
4. Gate: 60 fps sustained /video on kryptos with gpuShare 0.01, egress
   still ~10 kB/s-class, IDR-on-join and /video-key behavior identical to
   the minih264 path (the gs-bridge contract must not notice the swap).
