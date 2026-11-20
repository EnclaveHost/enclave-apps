# Direct Q8 route

Status, 2026-11-20: development work on the branch `kvm-box/shield-opencl-perf`,
host-container tested against a dedicated test worker. Not built into a
measured image, not run in a confidential VM and not deployed.

The queued route in [Q8_MASKED.md](Q8_MASKED.md) made the standard OpenCL path
correct. It was about fifty times slower than the natively integrated engine on
the same worker, because every launch paid for work that does not depend on the
launch. The direct route keeps the same decisions and the same arithmetic and
moves that work off the per-launch path. On two Qwen models it brings
generation from about 2 tokens per second to between 91% and 94% of the native
Shield path at the service's settings; the measurements and their limits are
below.

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

8. **A feed-forward continuation computed ahead.** A feed-forward block gates
   the pair of products together and multiplies the result by a third tensor.
   The executor watches for that: after a pair, it gates the two products
   itself (ggml's own `swiglu` on its CPU backend, in either operand order) and
   compares the bytes with the activation of the request that follows. Once a
   pair has been followed that way, the executor computes the third product on
   a second thread as soon as the pair is done, while the client is still
   doing the same arithmetic. It answers from that product only when the next
   request names the same tensor and its activation is byte for byte the one
   the product was computed from; anything else is computed normally and the
   expectation is dropped (three misses end it for that pair). On by default
   with four or more processors (`SHIELD_Q8_AHEAD=0` or `1` overrides): on two
   it has no processor to run on and costs a little.
9. **Local products keep their threads.** A product the executor declines is
   computed in the client (see below); its threads are now started once, and
   the row kernel has an AVX-512 form. The product is bound by how fast the
   weights arrive from memory, as it is in the native engine.

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

- The continuation is ordinary arithmetic on values the executor already
  holds, followed by a product the same client was about to ask for with the
  same bytes. The worker sees the exchange it would have seen. A product
  computed ahead and not asked for is discarded; it has used pads and is
  counted as a verified exchange. The gating graphs read the pair through
  separate input tensors: built on the products themselves they would contain
  the two multiplications, and the CPU backend would compute them again,
  unprotected and over the verified values (found in testing, where it showed
  as products that no longer matched what the client had been given).
- The second thread uses the engine only while the serving loop is blocked
  waiting for it; a backend failure there retires the executor exactly as one
  on the serving thread does.

The executor's request cap no longer applies to sessions (it reuses its graphs
instead of building one per request); the stdin protocol keeps its cap.

## Settings that decide the speed

**The inline CPU driver.** With the native CPU fallback, llama leaves only
buffer transfers and Q8 launches on the OpenCL device, and the CPU runtime's
inline driver (`POCL_DEVICES=basic`) serves those without a thread hand-off per
command. The packaged `llama-cli` wrapper defaults to it; `sd-cli` keeps the
threaded driver for its kernels. The queued routes are not reliable under the
inline driver: a wait on a queued launch returns before its output is written
(seen with the Q8 fixture). Under that driver the frontend keeps queued
launches on the CPU kernel.

**Fused attention.** With its layers on the OpenCL device llama leaves
attention unfused unless asked. Attention runs on the CPU either way; fused it
is about 1.5 ms a token faster on the 3B model. The wrapper passes
`--flash-attn on`; an option on the command line still wins.

**Idle OpenMP threads.** An engine's second OpenMP thread spins while the
engine waits for the worker. The desktop guest has two processors, and there
the spinning thread takes one from the thread that makes the pads. This is not
specific to the OpenCL route: the native engine, as shipped, runs at 59 tokens
per second on the 0.5B model on two processors and at 107 with
`OMP_WAIT_POLICY=passive` (3B: 14.6 and 22.3). With eight processors the
passive policy changes nothing at the service's settings and costs about 7%
when pads are plentiful. The service and the `llama-cli` wrapper now set the
passive policy when fewer than four processors are available.

## Executed evidence

Host container (`enclave-kvm-box-desktop:0.6.11-declines-candidate-v2` with the
rebuilt frontend, executor and `service.py`), AMD EPYC 9115, two llama
threads, 148 generated tokens, medians of alternating runs. The container is
pinned to whole cores (`--cpuset-cpus`); unpinned, single comparisons moved by
five percent and more. The test worker is a separate `shielded-worker` on a
Tesla V100 over loopback TCP. The earlier measurements in this file's history
used an RTX 3070 Ti that the desktop also used; the ratios were the same.

Generation, tokens per second, with the settings above applied to both paths:

| Model, processors, refill threads | Native Shield | Standard OpenCL | Ratio |
| --- | ---: | ---: | ---: |
| Qwen2.5-0.5B Q8_0, 2, 1 (the desktop guest's shape) | 100–102 | 92–94 | 91–94% |
| Qwen2.5-3B Q8_0, 2, 1 | 20.4 | 19.1 | 93.5% |
| 0.5B, 8, 1 | 115 | 107 | 93% |
| 3B, 8, 1 | 21.5 | 20.1 | 94% |
| 0.5B, 8, 4 | 158 | 141 | 89% |
| 3B, 8, 4 | 53–55 | 45 | 81–85% |

For scale: before this work the queued route gave 2.1 tokens per second on the
0.5B model, and the native engine as shipped (spinning OpenMP thread) gives 59
on two processors.

Prompt processing is faster on the OpenCL route (0.5B: 218 against 136 tokens
per second on two processors; 3B: 58 against 22). Start-up is slower: a whole
148-token run takes 6.7 s against 4.1 s on the 0.5B model and 26 s against
22 s on the 3B model, nearly all of it building OpenCL programs and reading
the weights into device buffers.

Where the remaining time goes, 3B model with pads plentiful (22.1 ms a token
against 18.1 ms native), from a trace of the executor's session loop:

| Per token | Native | OpenCL route |
| --- | ---: | ---: |
| Shield products (masking, worker round trip, verification) | 13.6 ms | 14.8 ms |
| Everything else | 4.5 ms | 7.3 ms |

The backend itself runs at the same speed in both processes. The executor adds
about 5 µs a request for the NaN fill and the checked copy into shared memory,
and the continuation adds its gating. The client adds about 16 µs around every
launch: two wake-ups, four buffer commands through the CPU runtime, nineteen
kernel arguments, and the scheduler's switch between its CPU and OpenCL
splits. With one refill thread both paths wait for pads for half of each
token, and an extra millisecond of work costs half a millisecond; that is why
the ratio is higher at the service's settings.

Tried and not kept, each measured on the 3B model: spinning instead of
sleeping on either side of the session (slows the worker round trip, which
shares the processors); pinning the executor to the client's processor or to
its SMT sibling (no gain, or much worse when the two were not both pinned);
two refill threads on two processors (slower for both paths); lowering the
priority of the executor's pad thread (within noise).

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
- `test_q8_session.py`: ten end-to-end cases against the real executor. One
  drives the continuation with a zero activation (whose gated product is zero
  on any CPU, so the expectation is certain to be learned): four products are
  answered from work done ahead and equal the request protocol's; a different
  activation, and a different tensor, after the same pair get their own
  products.
- Generated text is identical with the continuation on and off (3B, 148
  tokens), with 5,178 products answered ahead in the run.
- `test_q8_admission_native.py`: native admission equals the Python reference.
- A native CPU job started in the middle of a 300-token generation stops the
  executor; the client reconnects and the generated text is unchanged.
- The earlier Python suites (`test_q8_service`, `test_q8_finite`,
  `test_service`, `test_service_admission`, `test_compiler`) pass.

## What is not established

- Nothing here ran in a measured image or a confidential VM, on NucBox, or
  through the native profile's launcher and broker. The two-processor numbers
  come from a container pinned to two cores, not from the guest.
- Parity is not reached. At the service's settings the route is 6–9% behind
  the native path; with pads plentiful it is 11–19% behind.
- Two models, both Qwen2.5 Q8_0. The 3B model has one feed-forward tensor the
  calibration refuses (`blk.2.ffn_down.weight`); both paths compute it on the
  CPU.
- EyesOff's production model is a 27B Q4_K GGUF. This route cannot run it: only
  the Q8_0 kernels are lowered, and llama's OpenCL backend has no K-quant
  kernels to lower. A comparison with EyesOff itself needs a Q4_0 or Q8_0 file
  of a larger model and, for Q4_0, new kernel recognition and admission.
- The continuation is learned only for a pair gated with `swiglu` and only if
  the executor's ggml produces the same bytes as the application's. It did for
  the packaged llama; another build, or another activation function, simply
  never learns it and runs as before.
- The passive OpenMP policy changes the native engine's timing too. It was
  measured on these two models only.
- The executor now needs `-pthread` when it is linked.
- Only the two Q8 kernels are lowered. Other kernels, other weight types and
  CUDA binaries are unchanged.
- The new code has had no independent security review.
