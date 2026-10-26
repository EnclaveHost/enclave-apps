# Standard OpenCL numerical validation

The installed 0.6.9 runtime was compared with both original CPU arithmetic and
the native masked SDK in isolated host containers using the dedicated test
worker. The image remains
`sha256:94c1f667a5b2023a8188e5f28cee7bcc3d27706996855b522882c89a33c9e8e3`.
These are broader diagnostic fixtures, not general quality acceptance or an
end-to-end speedup. Confidential VM execution evidence is documented separately
in `STANDARD_PACKAGE.md`.

## Language model

`evaluate_standard_logits.cpp` teacher-forces four public texts through the
packaged Qwen2.5 0.5B Q8 model. CPU, native masked and standard OpenCL use the
same batch/ubatch of 32, two threads, CPU KV storage, disabled flash attention
and disabled extra CPU buffer types. The evaluator never ships in the VM.

All 40,111,104 logits across 264 tokens match byte-for-byte between standard
OpenCL and the native masked SDK. The comparison uses complete output-file
SHA-256 digests, including token IDs and all logits. This is agreement between
two implementations of the explicit approximate profile, not equality with
original CPU arithmetic.

| Public fixture | Predicted positions | Perplexity ratio, masked / CPU | Top-1 agreement |
| --- | ---: | ---: | ---: |
| Science | 57 | 1.02063 | 87.72% |
| Reasoning | 68 | 1.10173 | 91.18% |
| Code | 68 | 1.02719 | 89.71% |
| Multilingual | 67 | 1.00297 | 95.52% |

Position-weighted aggregate perplexity is 15.429 on CPU and 16.020 through the
masked profile, an increase of 3.83%. Aggregate top-1 agreement is 91.15%.
These 260 predicted positions do not establish general language-model quality.
Each run checks finite logits and identical token sequences. The test removes
large public logit files after analysis while retaining their hashes and
metrics. Its isolated container permits 128 MiB diagnostic files; production
service limits are unchanged.

Single-run standard-route times are 32.8–43.7 seconds, including startup,
compared with 0.56–0.67 seconds for CPU and 2.61–2.72 seconds for the native
masked SDK. These runs demonstrate a substantial compatibility overhead.
Verified exchange counts may differ between the standard and native paths
because their chunk shapes and verified-group reuse differ; output equality
is checked independently of those counts.

## Stable Diffusion

`evaluate_standard_images.py` uses four public prompts and seeds: portrait 17,
geometry 29, landscape 53 and typography 71. Every route uses SD 1.5 Q8,
256x256, eight Euler steps, CFG 7, two CPU threads and the same attention graph.
All four standard OpenCL PNGs are byte-identical to the native masked SDK.
Every masked run records 1,136 verified exchanges and 181,525,282,816 MACs.

| Image | Pixel MAE / 255 | PSNR |
| --- | ---: | ---: |
| Portrait | 13.0257 | 20.1556 dB |
| Geometry | 3.9800 | 27.5989 dB |
| Landscape | 10.0876 | 22.3257 dB |
| Typography | 5.0617 | 24.2964 dB |

CPU takes approximately 24 seconds, native masked SDK 28–30 seconds and
standard OpenCL 67–79 seconds. There is no demonstrated end-to-end speedup.
Pixel differences from CPU reflect the explicitly selected fixed-point
arithmetic; the saved image comparison permits visual review. Matching the SDK
on four fixtures does not establish general perceptual quality.

## Performance investigation

Claude reviewed weight placement and service overhead using restricted,
source-only contexts. The current loader already checks operation support
before allocating weights. Scheduler logs show no weight/bias split inputs for
the Qwen diagnostic; CPU and OpenCL model-buffer totals remain the same with an
explicit attention-to-CPU override. No loader patch is justified by this
experiment. An initial timing comparison reused the native broker and started
with a cold compiler cache, so it cannot establish a placement speedup. The
revised diagnostic warms the compiler cache and resets the broker between
processes. Both controlled runs record 511 exchanges and 14,478,327,808 MACs;
times are 36.11 seconds without the override and 35.84 seconds with it. Neither
run has weight/bias split inputs, and both retain 182.84 MiB CPU / 455.93 MiB
OpenCL model buffers. This supports leaving the loader unchanged.

Claude also identified Python's per-float finiteness loop as a candidate cost.
The source candidate uses NumPy's CPU `isfinite` over an explicit little-endian
float32 view. It retains complete input/output validation; no authentication or
result cache is added. NumPy is already present in the measured guest runtime.
The 2 MiB synthetic microbenchmark fell from roughly 25.5 ms to 0.042 ms. That
is a validator microbenchmark, not an application speedup.

Exhaustive upper-word classification with three lower-mantissa patterns covers
196,608 IEEE-754 inputs. Additional tests put infinities and NaNs at multiple
positions in maximum-size frames and check finite extremes and partial words.
The service/protocol/admission suite has 37 passing tests. Integration and fault
receipts distinguish this source-only, host-mounted candidate from the deployed
0.6.9 image. It must be packaged and tested in a new measured image before
promotion. The host override also passes ordinary SD/llama and native-service
checks. Corrupt and disconnected worker replies fail closed: no image is
published, the service retires, and retry is refused. The two-step SD check
takes 45.65 seconds and the llama check 39.64 seconds; these single-run times
do not establish a reliable application speedup.

## Evidence and limits

The task exports `standard-numerical-progress.json`, a SHA-256 evidence manifest
and a self-contained `standard-image-comparison.html`. The receipts include
exact immutable image IDs, commands, model/policy identities, evaluator source,
fixture text, output hashes, telemetry, timings and Claude's review proposals.
GPU jobs run serially on test worker 9741. No production GPU worker or live VM
runtime is changed by these host tests.

Broader API coverage, performance, quality acceptance, production identity
binding and publication remain open. Strict CPU arithmetic remains available;
masked execution still requires the explicit `shield-fixed-point-v1` opt-in.
