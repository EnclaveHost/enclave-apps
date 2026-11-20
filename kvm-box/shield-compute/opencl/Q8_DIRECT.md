# Direct Q8 route

Status, 2026-11-19: development work on the branch `kvm-box/shield-opencl-perf`,
host-container tested against the dedicated RTX test worker. Not built into a
measured image, not run in a confidential VM and not deployed.

The queued route in [Q8_MASKED.md](Q8_MASKED.md) made the standard OpenCL path
correct. It was about fifty times slower than the natively integrated engine on
the same worker, because every launch paid for work that does not depend on the
launch. The direct route keeps the same decisions and the same arithmetic and
moves that work off the per-launch path. On the Qwen fixture it brings
generation from about 2 tokens per second to roughly nine tenths of the native
Shield path; the measurements and their limits are below.

## What a launch used to cost

Measured per launch of a recognized Q8 kernel, before any change:

| Stage | Cost | Why |
| --- | --- | --- |
| Enqueue | 2.9 ms | a helper process per new launch layout, a cloned kernel, a new command queue, events |
| Weight snapshot and hash | 4.5 ms | the whole weight tensor was read back and SHA-256 hashed on every launch |
| Service request | 5.1 ms | JSON and base64 through the Python service, then a pipe to the executor |

Two costs were paid once per run but were large: the executor registered each
tensor with the worker as it was first used, and every registration restarted
the link with all tensors registered so far (12 s for Qwen's 73 tensors); and
each launch layout was validated by starting Python (about 10 s over a run).

## What changed

1. **Weight identity is remembered** (`q8_direct.inc`). A weight region is
   hashed when first used. The answer is kept until something could have
   written to the region. Every OpenCL call that can write a buffer drops the
   overlapping entries: buffer and image writes, fills, copies, rect variants,
   maps for writing (and for as long as they stay mapped), unmaps, sub-buffers
   and buffer-backed images, any other kernel given the buffer, native
   kernels, migration and release. Buffers created over host memory are never
   remembered. A request for an extension entry point, which can enqueue work
   this library does not see, turns remembering off for the process.
2. **Executor sessions** (`native/q8_executor.cpp`, `q8_service.py`,
   `service.py`). The service checks the client's credentials as before, then
   passes the connected socket to the executor. The client speaks a small
   binary protocol to it directly. Payloads travel through memory the executor
   creates and seals (`memfd`, shrink/grow/seal, non-executable where the
   kernel supports it); a client that cannot use it sends them on the socket.
3. **Launch admission in C++** (`q8_admission.inc`). The same checks as
   `q8_admission.py`, which remains the reference; a test compares the two on
   thousands of launches.
4. **One registration.** At the first session the executor offers every
   approved tensor to the backend, so the link starts once. It does this only
   when the whole approved set fits its share of the card with a fifth to
   spare, because the backend refuses a placement it cannot complete.
5. **The launch runs inside the enqueue call.** Earlier commands on the queue
   are finished, the activation is read, the product is written and the call
   returns. This applies to in-order, non-profiling queues and launches without
   a wait list. Everything else keeps the queued route.
6. **Paired products.** A feed-forward gate and up projection take the same
   activation. Once the client has seen one tensor follow another with
   byte-identical input, it asks for both in one request; the executor runs
   them as one graph. The second product is used only if the next launch names
   that tensor with those exact bytes. A miss costs one unused product and the
   pair is forgotten.

7. **A warm-up hint.** At its first program build a process that has opted into
   masked arithmetic tells the service so. The service starts the executor for
   the engine used last and lets it register its tensors while the application
   is still compiling kernels. Nothing is computed; a wrong guess costs the
   later session one executor restart.

`SHIELD_OPENCL_Q8_DIRECT=0` selects the queued route; `SHIELD_OPENCL_Q8_SHARED=0`
keeps payloads on the socket and disables pairing.

## Why this keeps the boundary

- The remembered identity only says which public tensor the application means.
  The service still computes with its own authenticated copy of that tensor, so
  a missed write could give an application a product for the original weights.
  It could not disclose weights or activations. `test_q8_identity.cpp` changes
  the weights through eleven write paths and requires the CPU kernel each time.
- The application process still holds no worker route. The executor runs as
  the service identity; the session socket is created by the service's
  `accept` and reaches the executor only after the `SO_PEERCRED` check.
- The executor now reads client bytes itself. A client mistake (bad frame,
  unknown slot, an unapproved tensor reference, non-finite input, a stall of
  ten seconds) ends or refuses that session only. Backend, model and accounting
  failures still retire the executor and the service. Inputs are copied out of
  shared memory before they are checked; outputs are copied in after.
- A planned stop (a native or graph job taking the worker) sends each session
  a goodbye frame between requests. A client that sees it reconnects and
  resends; a session that ends without it is a failure and is not retried.
- Failures are still fail-closed. With an event, a failed launch completes its
  event with an error as the queued route does. Without one, the enqueue call
  returns the error; the queued route cannot report such a failure at all.

The executor's request cap no longer applies to sessions (it reuses its graphs
instead of building one per request); the stdin protocol keeps its cap.

## The inline CPU driver

With the native CPU fallback, llama leaves only buffer transfers and Q8 launches
on the OpenCL device, and the CPU runtime's inline driver (`POCL_DEVICES=basic`)
serves those without a thread hand-off per command. The packaged `llama-cli`
wrapper now defaults to it; `sd-cli` keeps the threaded driver for its kernels.

The queued routes are not reliable under the inline driver: a wait on a queued
launch returns before its output is written (seen with the Q8 fixture). Under
that driver the frontend therefore keeps queued launches on the CPU kernel.

## Executed evidence

Host container (`enclave-kvm-box-desktop:0.6.11-declines-candidate-v2` with the
rebuilt frontend and executor), dedicated worker on an RTX 3070 Ti that the
desktop session also uses, AMD EPYC 9115, Qwen2.5-0.5B Q8_0, two llama threads,
default service settings (one refill thread). Medians of alternating runs.

| 148 generated tokens | Native Shield | Standard OpenCL, direct | Queued route |
| --- | ---: | ---: | ---: |
| Generation, 4 CPUs | 114 t/s | 104 t/s (92%) | not measured |
| Generation, 2-CPU quota | 84 t/s | 76 t/s (91%) | 2.1 t/s |
| Prompt, 4 CPUs | 129 t/s | 346 t/s | 6.5 t/s (2 CPUs) |
| Whole run, 4 CPUs | 3.2 s | 5.9 s | 60 s (2 CPUs, 48 tokens) |

The queued-route column is the packaged 0.6.11 candidate before this work.

Runs differ by a few percent with what else the card and host are doing; single
comparisons between 88% and 99% were seen.

The same comparison under other service settings (generation, tokens per second):

| Worker transport, refill threads, CPUs | Native Shield | Direct route | Ratio |
| --- | ---: | ---: | ---: |
| TCP, 1, 4 (the packaged default) | 112–114 | 100–104 | 89–92% |
| Shared memory, 1, 4 | 114 | 110 | 97% |
| TCP, 2, 4 | 180 | 133 | 74% |
| Shared memory, 2, 8 | 200 | 153 | 77% |

With one refill thread both paths wait for pads and the difference nearly
disappears. With pads plentiful the native path is ahead by about 1.5 ms a
token, and that difference is not the number of round trips: an experiment that
let the executor compute a layer's gated product itself and start the down
projection early removed a third of the remaining requests, matched llama's
bytes exactly, and changed nothing measurable, so it was taken out again. What
remains is spread over the OpenCL calls llama makes around each launch, the
copies of each activation and product, and a slightly longer wait for the
worker in a process that sleeps between requests.

- The acceptance prompt returns the Paris answer in 5.3 s with 487 verified
  exchanges and 14,273,495,040 MACs. The earlier 511 belonged to tensors being
  registered one at a time; a warm service gave 487 before this change too.
- Stable Diffusion gives the same PNG
  (`048b2445…7abf80`), 392 exchanges and 51,921,289,216 MACs on both routes.
- Generated text is byte-identical across the queued route, the direct route
  under either CPU driver, and the direct route without shared memory.
- `probe_q8_masked.py`: both kernels match the native fixed-point reference
  with intact guards on both routes; a corrupted worker reply and a dropped
  worker connection each fail the launch, leave its output untouched, retire
  the service and refuse a retry, on both routes.
- `test_q8_identity.cpp` (via the probe): eleven write paths, each followed by
  CPU execution and, after restoring the bytes, the native product again; the
  service counts exactly one exchange per approved launch. With invalidation
  disabled the test fails at the first write.
- `test_q8_session.py`: nine end-to-end cases against the real executor.
- `test_q8_admission_native.py`: native admission equals the Python reference.
- A native CPU job started in the middle of a 300-token generation stops the
  executor; the client reconnects and the generated text is unchanged.
- The earlier Python suites (`test_q8_service`, `test_q8_finite`,
  `test_service`, `test_service_admission`, `test_compiler`) pass.

## What is not established

- Nothing here ran in a measured image or a confidential VM, on NucBox, or
  through the native profile's launcher and broker.
- One small model. Each exchange costs about 35 µs more than in the native
  engine (the extra process hop and the OpenCL calls around it). That is a
  tenth of a token for this model at default settings; its share for a larger
  model has not been measured.
- With pads plentiful (two or more refill threads) this route reaches about
  three quarters of the native path on this model; see the table above.
- EyesOff's production model is a 27B Q4_K GGUF. This route cannot run it: only
  the Q8_0 kernels are lowered, and llama's OpenCL backend has no K-quant
  kernels to lower. A comparison with EyesOff itself needs a Q4_0 or Q8_0 file
  of a larger model and, for Q4_0, new kernel recognition and admission.
- Startup is about 2.7 s longer than native, nearly all of it building OpenCL
  programs with a warm CPU-runtime cache (much longer cold).
- Only the two Q8 kernels are lowered. Other kernels, other weight types and
  CUDA binaries are unchanged.
- The new code has had no independent security review.
