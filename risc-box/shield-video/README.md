# Enclave Shield video: masked H.264 transforms

This is an opt-in, working **partial video-encoding offload** for RISC Box. A GPU
worker receives fresh masked input blocks and returns integer transforms. The
trusted app verifies and unmasks them, then produces an ordinary H.264 stream.
Prediction, motion search, quantization, entropy coding, reference frames and
bitstream assembly stay inside the isolated app guest.

It does **not** make NVENC safe for an untrusted GPU, offload the whole encoder,
or yet accelerate encoding over the existing CPU implementation. Do not advertise
production GPU availability from this component alone.

## Use

Build the RISC Box command component:

```sh
cargo build --locked --release --target wasm32-wasip2 \
  --manifest-path risc-box/Cargo.toml --features masked-video
```

Add this object to the existing `ENCLAVE_CONFIG` (retain the app's other settings):

```json
{"shield_video":{"worker":"127.0.0.1:9518"}}
```

Alternatively set `ENCLAVE_SHIELD_VIDEO_WORKER=127.0.0.1:9518`. The environment
variable takes precedence. Select H.264 in the video client. Explicit Shield
configuration disables NVENC probing/selection. A missing build feature, bad
endpoint, unsupported codec, failed worker or invalid result refuses/stops the
encoder; it does not route raw frames to NVENC. A new encoder instance reconnects
with new masks. GameStream uses the same encoder selection, but requires its
existing port plumbing and is not claimed to meet its real-time target.

The address must be reachable through an operator-provided, constrained worker
connection. `127.0.0.1` is for a local test or an in-guest proxy, **not** a route
that automatically reaches the physical host. The production per-app SNP runtime
currently lacks that worker connection and the extra RISC Box TCP/UDP ports.
This change does not remove its GPU admission checks or enable that deployment.
Keep the worker listener private; this protocol does not authenticate tenants or
enforce their resource allocations. Those are transport/scheduler responsibilities.

## Existing streaming standards

[SRTP (RFC 3711)](https://www.rfc-editor.org/rfc/rfc3711) protects RTP payloads;
[SFrame (RFC 9605)](https://www.rfc-editor.org/rfc/rfc9605.html) protects encoded
media end to end through forwarding servers. Neither lets an untrusted stock
video encoder operate on ciphertext while hiding its input pixels. For useful
video today, CPU encoding inside the isolated guest followed by a standard
encrypted transport is the simpler path and is faster in the measurements below.
This module explores the separate problem of confidential GPU computation.

## Construction and trust boundary

For each aligned 4x4 input block, the H.264 integer transform is `T X T^t`, with
rows of `T` equal to `[1,1,1,1]`, `[2,1,-1,-2]`, `[1,-1,-1,1]`, `[1,-2,2,-1]`.
It is linear before quantization. We send `X+R` in the existing worker's CRT ring
`Z/(251*241*239)`, subtract the locally computed transform of `R`, and later
subtract the transform of the private predictor. The modulus is 14,457,349,
well above the exact transform bounds.

- Every one of the 32 lanes, including unused lanes and padded blocks, gets a
  fresh independent, rejection-sampled OS-CSPRNG mask. No reusable pad, seed or
  plaintext input is sent to the worker.
- Every batch has 2,048 blocks. Every frame sends all raster-order blocks in all
  three I420 planes, including a padded final batch. There are no network cache
  shortcuts for repeated pixels. Edge pixels are clamped inside the guest.
- Public, fixed transform weights use the existing q8_0 FIELD_GEMM graph. The
  coefficient scale is exactly 1/256, matching the worker's field encoding.
  This needs no new worker opcode or arbitrary GPU program.
- Results must have the exact length, canonical field representation, bounded
  coefficients and zero unused output lanes. After the entire response arrives,
  five fresh random Freivalds challenges check every row, including padding,
  over the prime 2,147,483,647. No coefficient enters the codec before all checks
  pass. For a fixed incorrect bounded result, the algebraic false-acceptance
  bound is at most `p^-5` per batch, assuming independent uniform challenges
  and a correct trusted implementation. This is not a whole-system security proof.
- A frame-local cache maps private input blocks to verified transforms. Codec
  cache misses (for example denoised/cropped variants) use the original transform
  locally. They never send an extra, content-dependent GPU request.
- Failed public frame operations poison the connection. I/O has five-second
  socket timeouts and bounded response allocation; no response-controlled buffer
  allocation or retry of an old mask. Timeouts are socket inactivity timeouts,
  not an overall wall-clock deadline against a trickling peer.

The host/worker can still observe dimensions, batch count, timing, cadence,
connection failures and traffic volumes. This does not promise side-channel
obliviousness, availability, GPU memory isolation or protection from compromised
code/randomness **inside** the trusted guest. Full-frame contents, pads,
predictors, references and encoded output remain private to that guest until the
app sends its video to an authorized viewer. Existing app access controls and
transport encryption still matter. Cryptographic and implementation review are
required before making broader product security claims.

## Integration

`src/lib.rs` implements the trusted client and scoped per-thread coefficient
cache. `vendor/minih264/wrapper.c` enables a marked scalar-transform hook only for
`masked-video`. Non-Shield builds retain the original SIMD/transform selection.
Shield-enabled native builds use the portable C codec even when no endpoint is
configured; WASM already uses that codec. The hook never performs I/O. The Rust
encoder prepares and verifies a complete frame before changing H.264 state.

## Validation

Unit/adversarial transport tests, no GPU needed:

```sh
cargo test --locked --release --manifest-path risc-box/shield-video/Cargo.toml
```

Tests cover transform basis/extremes, fresh masks and fixed wire sizes, corrupted,
replayed, noncanonical, truncated and oversized replies, disconnects, invalid
frame shapes, rejection of further work after failure, and scoped-cache cleanup.

For real-GPU tests, start a **dedicated** existing CUDA Shield worker on an idle
GPU and loopback port (adjust binary/GPU paths to the machine):

```sh
CUDA_VISIBLE_DEVICES=2 /path/to/shielded/worker-cuda/shielded-worker \
  --host 127.0.0.1 --port 9518 --vram-gb 0.1 --quiet
SHIELD_VIDEO_WORKER=127.0.0.1:9518 cargo test --locked --release \
  --manifest-path risc-box/shield-video/Cargo.toml --features codec-tests \
  --test codec -- --ignored --nocapture
SHIELD_VIDEO_WORKER=127.0.0.1:9518 SHIELD_VIDEO_LARGE_TEST=1 \
  cargo test --locked --release --manifest-path risc-box/shield-video/Cargo.toml \
  --features codec-tests --test codec -- --ignored --nocapture
```

Eight frames per case must match the CPU bitstream byte for byte and decode with
FFmpeg without errors. Cases are 64x64, 96x64, cropped 66x50, and separately
1024x768. They include changing content and a forced midstream keyframe; the test
asserts that verified GPU coefficients actually reached the codec.

The same codec probe also runs through WASI sockets and randomness:

```sh
cargo build --locked --release --target wasm32-wasip2 \
  --manifest-path risc-box/shield-video/Cargo.toml \
  --features codec-tests --example codec_probe
mkdir -p /tmp/shield-video-probe
wasmtime run -S inherit-network=y -S tcp=y \
  --dir /tmp/shield-video-probe::/out \
  --env SHIELD_VIDEO_OUTPUT_DIR=/out \
  --env SHIELD_VIDEO_WORKER=127.0.0.1:9518 \
  --env SHIELD_VIDEO_LARGE_TEST=1 \
  risc-box/shield-video/target/wasm32-wasip2/release/examples/codec_probe.wasm
ffmpeg -v error -i /tmp/shield-video-probe/enclave-shield-video-0-1024x768.h264 -f null -
```

Omit `SHIELD_VIDEO_LARGE_TEST` for the smaller cases. The broad network permission
above is for this local synthetic-data probe only, not production guest policy.

## Measurements, 2026-09-27 UTC

Local Tesla V100 PCIe 32GB, dedicated existing CUDA worker, synthetic 1024x768
I420 frames, eight frames, identical portable-C codec/quality settings:

| Runtime | CPU codec total | Mask + GPU + verify + codec total | Masked encode rate |
| --- | ---: | ---: | ---: |
| Native release | 52.8 ms | 788.7 ms | 10.1 fps |
| Wasmtime / wasm32-wasip2 release | 100.2 ms | 1,032.0 ms | 7.8 fps |

These are single short local runs, not production or sustained desktop results.
Timers exclude input generation, comparison, file writing, FFmpeg and worker
connection setup, and include all masking/verification work in the masked time.
Both paths encoded exactly the same frames. The native reference is portable C,
not the faster default native SIMD codec. Each run used 288 batches and 589,936
codec cache hits, with no misses at 1024x768. Smaller cropped frames exercise the
private CPU miss path. All four resolutions also decoded from the WASM output.

This is a functional protected-offload starting point, **not a speed win**.
The transform is cheap; random masks, verification and transfers dominate.
Useful acceleration requires offloading more expensive work or changing the
codec partition while preserving its security properties. A live production
release additionally requires constrained guest-worker transport, resource
admission/accounting, RISC Box ports, and a measured/admitted app build.
