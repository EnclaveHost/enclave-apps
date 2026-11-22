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

10. **Programs are built when a kernel is used** (`SHIELD_OPENCL_LAZY_BUILD=1`,
    set by the packaged `llama-cli`). llama builds 119 programs and creates
    their kernels, then launches kernels from five. A build from the CPU
    runtime's cache costs about 25 ms. A program whose source is in the
    packaged table of sources with nothing to lower is not built when asked;
    its kernels are handles of this library until one is used for anything
    but retain and release, and then the program is built and the handle
    stands for the real kernel. A build or kernel error is reported at that
    later call, which is why an application has to opt in. The warm-up hint
    now goes out at the first device query.

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
| Qwen2.5-0.5B Q8_0, 2, 1 (the desktop guest's shape) | 100–103 | 92–96 | 91–94% |
| Qwen2.5-3B Q8_0, 2, 1 | 20.4 | 19.1 | 93.5% |
| 0.5B, 8, 1 | 115 | 107 | 93% |
| 3B, 8, 1 | 21.5 | 20.1 | 94% |
| 0.5B, 8, 4 | 158 | 141 | 89% |
| 3B, 8, 4 | 53–55 | 45 | 81–85% |

Over 440 generated tokens, where the pads made before the first token no
longer matter, the two-processor rows are 85 against 90 (95%) and 18.2
against 19.0 (96%). On two processors both paths keep both busy: the pad
thread has one to itself, and the rest of the OpenCL route (client and
executor together) uses about 1.3 ms more processor time a token than the
native engine on the 0.5B model.

For scale: before this work the queued route gave 2.1 tokens per second on the
0.5B model, and the native engine as shipped (spinning OpenMP thread) gives 59
on two processors.

Prompt processing is faster on the OpenCL route (0.5B: 250 against 138 tokens
per second on two processors; 3B: 58 against 22), because the executor has
been making pads since the hint.

A whole run of 39 prompt tokens and 148 generated ones, two processors, in
seconds. The native engine registers the model with the worker in every run.
The executor does it once and stays; a native job in between displaces it.

| | Native Shield | OpenCL, executor not running | OpenCL, executor running |
| --- | ---: | ---: | ---: |
| 0.5B | 4.1–4.5 | 4.4–4.6 | 3.3 |
| 3B | 21.3–21.5 | 25.5–25.7 | 14.0 |

Before the programs were built on demand the 0.5B run took 6.7 s. On the 3B
model a first run is slower than native because the client hashes every weight
it uses (1.8 s) and the executor checks every digest, on the same two
processors, before the registration that both paths wait for.

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

## Q4_K and Q6_K

EyesOff's model is a Q4_K_M file, which holds Q4_K and Q6_K tensors, so the
route was extended to the four kernels the OpenCL backend uses for them
(`kernel_mul_mm_q4_k_f32_l4_lm`, `kernel_mul_mv_q4_K_f32_flat`,
`kernel_mul_mm_q6_k_f32_l4_lm`, `kernel_mul_mv_q6_K_f32_flat`). Each takes four
weight buffers, which are the GGUF block split apart (quants, 6-bit scales,
`d`, `dmin` for Q4_K; low bits, high bits, scales, `d` for Q6_K). Three of the
four have the Q8_0 kernels' remaining arguments two places later; the Q6_K
vector kernel has its own, shorter list.

- The four sources are pinned in `q8-reference/` and recognised by the same
  structural comparison, each as its own operation. The Q6_K vector kernel
  does its arithmetic in a function it calls. The comparison now takes such a
  function's whole body as part of its caller, and still refuses a source
  with a function no kernel calls.
- Launch admission covers all six layouts in the Python reference and in C++;
  the parity test compares them on more than nine thousand changed launches.
- The frontend remembers a tensor's four buffers as one, tracks writes to any
  of them, and authenticates the tensor by its GGUF digest recomputed from
  them. Direct route only.
- A K-quant tensor the executor declines is computed in the client like a
  Q8_0 one, from codes, scales and (Q4_K, Q5_K) minimums decoded from the flat
  buffers. `test_q8_kquant.py` checks the decoding against gguf-py's.
- Executor sessions accept approved Q4_K, Q5_K and Q6_K tensors whose rows are
  whole 256-weight blocks. Q5_K, added after the measurements below, is Q4_K
  with a fifth weight buffer for the high bit of each quant.
- `llama-shield-cpu-fallback.patch` lets those products reach the OpenCL
  device. The plugin has to be rebuilt with it, and the packaged table of
  sources with nothing to lower has to be regenerated without the four
  sources. Both were done by hand outside this repository for the
  measurements below; no image contains them.

Qwen2.5-3B Q4_K_M (217 Q4_K tensors, 37 Q6_K, calibrated with
`shielded-calib`; the calibration refuses one Q6_K tensor), same harness and
worker as above, 148 generated tokens, tokens per second:

| Processors, refill threads | Native Shield | OpenCL route | Ratio |
| --- | ---: | ---: | ---: |
| 2, 1 | 20.9 | 19.1 | 92% |
| 8, 1 | 21.3 | 19.9 | 93.5% |
| 8, 4 | 57.2 | 44.4 | 78% |

Both paths make the same 3,741 verified exchanges for the first 24 tokens with
the same work, and the generated text is identical over all 148 tokens.
Prompt processing is 34 against 22 tokens per second on two processors. A
whole run with the executor not yet running takes 28.5 s against 25.7 s there.

With only Q4_K lowered, and the Q6_K tensors left on the CPU, the same model
ran at 104–112% of the native engine with one refill thread and at 51% with
four: fewer offloaded products need fewer pads, which helps when pads are
scarce and hurts when the CPU is the limit. Those numbers describe a different
split of the work, not a faster route.

## EyesOff's model

`Qwen3.8-27B-UD-Q4_K_XL.gguf` with the calibration EyesOff uses (262 sites),
two llama threads, eight processors, two test workers on the two V100s
through a worker pool (one card cannot hold the model). Of its 410 tensors
large enough to offload, 191 are Q5_K, 70 IQ4_XS, 69 Q4_K, 56 Q6_K, 14 Q8_0,
6 IQ4_NL, 3 Q3_K and 1 IQ3_S. All eight types are lowered now:

- Q8_0, Q4_K, Q5_K, Q6_K and IQ4_NL through the kernels llama's OpenCL backend
  has for them.
- IQ4_XS, Q3_K and IQ3_S have no kernel there. `llama-shield-iq4-xs.patch`
  adds one plain kernel for each to the backend, used only on the Shield
  device: the weights stay as the GGUF has them, one work item computes one
  output. The three sources are pinned in `q8-reference/` and recognised,
  admitted and authenticated like the others (the digest is of the buffer).

Tokens per second, 62 prompt tokens, medians of alternating runs:

| Refill threads | Generated | Native Shield | OpenCL route | Ratio |
| --- | ---: | ---: | ---: | ---: |
| 4 | 100 | 7.9 | 7.8 | 99% |
| 2 (one a card) | 64 | 4.9 | 4.8 | 98% |
| 1, eight processors | 48 | 1.8 | 1.4 | 76% |
| 1, two processors | 48 | 1.6 | 1.3 | 84% |
| 4, EyesOff's link settings | 100 | 6.3 | 6.2 | 99.6% |

The last row adds the settings EyesOff's deployment gives the link
(`SHIELDED_SPLIT_COLS=1`, which sends every product to both cards,
`SHIELDED_OVERLAP_VERIFY=1`, pool depth and refill batch 64, weight budget
0.95) to both paths.

The generated text is identical to the native engine's in every row, and the
offloaded work is the same (4.09e12 MACs for the 100-token run).

Three things got it there from 58% with four refill threads:

- **The fused Gated Delta Net.** llama turns the fused op off when it lands
  on another device than its layer, and on this route it lands on the CPU by
  design. Its unfused form was about 80 ms of `mul`, `sum_rows` and `repeat`
  a token, more than everything else the client did.
  `llama-shield-fused-cpu.patch` (in libllama, not the plugin) keeps the fused
  op when the layer's device is the Shield device with the native CPU
  fallback. Found with a wall-clock profile taken only after generation had
  begun; a profile that includes the prompt shows start-up work instead.
- **A third tensor on one activation.** Attention's q, k and v read the same
  bytes. The third was learned as the first one's pair and replaced the pair
  just served, so one product in three tokens was computed and thrown away,
  with its pads. It is now learned as the second one's follower.
- **The last four tensors.** Three Q3_K and one IQ3_S feed-forward tensors
  were 10 ms of CPU a token.

**One refill thread and two cards.** The backend divides the refill threads
over the cards, so with one thread the second card has none and every pad for
a tensor placed there is made while the engine waits (both paths: 31 to 34 s
of a 35 to 41 s run). Which tensors land on that card then decides the speed.
The backend places layers in name order on the emptier card. The native engine
offers the tensors llama's graph uses; the executor offers every approved
tensor, which here adds the five groups of the model's unused MTP block
(`blk.64`). That moves `output.weight` (a 58 ms pad a token) and one block to
the card without a refill thread. With a refill thread on each card the
difference is gone. The KVM Box service gives the engine one worker, not a
pool, so this is a property of this rig; it would matter for a pool.

**Cold start.** A tensor's first launch used to wait for its weights to be
read back and hashed: 13 s for this model, spent after the executor was
ready. The bytes are in place as soon as the application has loaded them,
while the executor is still starting, so they are now authenticated then:
when a kernel that is not a product has written sub-buffers whose sizes, in
argument order, are the flat buffers of a catalogued tensor, or the
application writes exactly a catalogued tensor's GGUF bytes, the frontend
authenticates them as a launch would and remembers the identity under the
same write tracking (`q8_preauthenticate_*` in `q8_direct.inc`). A wrong guess
costs the time and remembers nothing; after 64 of them it stops looking.
`SHIELD_OPENCL_Q8_PREAUTH=0` turns it off. With it all 401 tensors are
authenticated ahead and no launch authenticates; a 16-token run from a cold
start takes 99 s instead of 113 s, against the native engine's 91 s. The rest
is the upload to the OpenCL device, which the native engine does not do.

**When pads are in stock.** These rows are all limited by how fast pads are
made, ten refill threads included (7.7 against 6.8 with sixteen logical
processors on the same eight cores). The first tokens of a run, which use the
pads made while the model loaded, show what is left when they are not: 14.2
tokens a second against 12.3 (87%) over 16 tokens. The difference is a fixed
cost of about 35 us for each of some 255 round trips a token. Measured on the
3B model with timestamps in the shared memory: 7 us for the two crossings
between the processes (3 us when both sides spin, which costs more than it
saves on eight busy processors), about 10 us in the executor around the
backend call and 4 us in the frontend. The backend call itself takes what the
native engine's exchange takes. Pinning the two threads to one core's two
hardware threads was worth 7% in one arrangement and broke another; it is not
used.

Both paths are far below what EyesOff reaches in production (about 19 tokens
per second): this harness uses the KVM Box service's settings and two V100s,
not EyesOff's.

## When the pad pool is empty

At the service's settings one refill thread makes the pads, and it does not
keep up: on the 3B Q4_K_M model about one exchange in eight finds its pool
empty and makes its own pad inside the request. Those are the large groups
(the gate and up projections together take 2.1 ms, a down projection 1.2 ms,
the vocabulary projection 13 to 17 ms) and they are two thirds of a token's
time on either path. An exchange that finds its pad takes 0.05 to 0.15 ms.

The executor holds the same authenticated weights and has the CPU backend, so
it can compute such a product itself: ordinary arithmetic on values it
already holds, as the backend does for a tensor it has no site for. Nothing
leaves the process. `PadStock` in `native/q8_executor.cpp` decides for each
group of products. Until an exchange has had to make its own pad everything is
exchanged as before. From then on the group's refill rate is estimated (the
backend's count of pads made on the request path says after each exchange
whether the pool was empty), and when no pad is expected the product is
computed here if that is cheaper than the exchange is expected to be, by the
times measured for that group. A group that is cheaper here than an exchange
that finds its pad is computed here regardless. Session products of one row
only; the line protocol still exchanges everything and reports the backend's
accounting.

Measured on the 3B Q4_K_M model, three alternating runs each, service
settings, tokens per second in generation:

| processors | native engine | this route | |
|---|---|---|---|
| 2 (pinned) | 21.1 | 23.0 | 109% |
| 4 (pinned) | 21.4 | 25.7 | 120% |
| 8 (pinned) | 22.2 | 29.1 | 131% |
| 8, two refill threads | 36.9 | 36.6 | 99% |
| 8, four refill threads | 61.7 | 49.0 | 79% |

With pads plentiful nothing is computed here and the route is where it was.
The gain comes from processors the native engine leaves idle at these settings
(the client's while it waits, and any beyond the engine's two threads): the
product is computed on all of them, less one or two when there are more than
two. Before this the route was at 91 to 96% in the first three rows.

What it changes, and why a deployment may not want it:

- Fewer products are exchanged, so `verifiedOffloads` and `maskedMacs` count
  less than the work done: about half the products of a token at the
  service's settings. They still count exactly what was exchanged.
- Whether a product is exchanged depends on timing, and the two computations
  round differently. Equal requests no longer give equal bytes, and a run's
  text differs from the native engine's and from one run to the next (the
  text of every run read as sound). With every product exchanged the route's
  text equals the native engine's.
- The card does less. That is the point at these settings, where making the
  pad costs more than the product, but a deployment that wants the work on
  the card should turn it off or give the engine refill threads.

`q8ExchangeEveryProduct: true` in the service configuration (or
`SHIELD_Q8_DRY_LOCAL=0` for the executor) exchanges everything.
`SHIELD_Q8_DRY_LOCAL_THREADS` and `SHIELD_Q8_DRY_LOCAL_MARGIN` (0.35: how
much cheaper the product must be here) are for measuring;
`SHIELD_Q8_DRY_LOCAL_REPORT=1` prints the times per group when the executor
stops. `test_q8_session.py` runs its accounting tests with every product
exchanged and has one test of the default.

### EyesOff's 27B with this

Measured 2026-11-22 at EyesOff's link settings (pool depth and refill batch
64, four refill threads, two cards), 100 tokens, alternating runs, with the
buffers of the next section so that it fits beside the other tenants.

| Executor | OpenCL route | Native Shield |
| --- | ---: | ---: |
| every product exchanged | 6.31, 6.35, 6.32, 6.33 | 6.46, 6.26, 6.41, 6.46 |
| the estimate above alone | 6.28, 4.62, 4.95 | |
| with the cost rule below | 6.19, 6.20, 6.30 | 6.39, 6.37, 6.33 |

The estimate alone loses here. A pool this deep is refilled a batch at a
time, and a group's exchanges go 64 found, then 1 to 13 not, then 64 found
again; 3,335 of 25,444 exchanges find nothing. The estimate models a rate, so
it sent 3,900 to 11,100 products here, most of which would have found a pad
in 0.15 ms. And a product costs more here than the pad does: 6 to 8 ms on
eight processors for most groups, against 3 ms for the card to make the pad.

So a group is exchanged whatever the estimate says when its product costs as
much here as a pad made on the path (`SHIELD_Q8_DRY_LOCAL_WORTH`, the share
of that cost, default 1). Before a group has been tried, its cost here is
taken from the groups that have, by size, so a large tensor is not computed
once just to find out. With the rule one or two products a run are computed
here on this model and the route is where exchanging everything puts it, 98
to 100% of the native engine. On the 3B model at the service's settings the
rule changes nothing at a share of 1 (103 and 104% on two processors, 130% on
eight); a share of 0.5 costs it the gain on two processors (95 and 97%).

Counting the batches instead of estimating a rate was tried and not kept: it
was right about when the pool is empty and still lost, because the product is
dearer here than the pad.

What limits both paths on this model is the supply of pads. 400-token runs:

| Refill threads | Pool depth | Native Shield | OpenCL route | Ratio |
| --- | ---: | ---: | ---: | ---: |
| 4 | 64 | 7.14, 7.18 | 7.08, 7.01 | 98% |
| 4 | 128 | 7.38, 7.55 | 7.09, 7.40 | 97% |
| 6 | 64 | 7.98 | 7.47 | 94% |
| 8 | 64 | 7.63 | 6.97 | 91% |

A deeper pool leaves the misses where they were (8,300 to 9,100 of 102,544),
so the threads do not keep up; six of them do (one miss), and then the route
is behind by about 9 ms a token, 35 us for each of its 256 exchanges. Eight
is worse for both on eight processors. The route does not exceed the native
engine on this model in any of these.

## EyesOff's own settings and engine

The rows above compare the route with the same backend under this harness's
settings. EyesOff itself runs at about 20 tokens a second on these two cards
(`shielded/REPORT.md` in the platform repository, sections 16 to 18), so that
comparison was against a slow baseline. What its deployment and that report
use, and this harness did not: a shared-memory ring to each worker instead of
TCP, eight engine threads, a refill thread for every pool (16 in all), batch
width 64, no processor limit, and drafting with the model's MTP head through
`libenclave_llama`.

Measured 2026-11-22 on a loaded host (another tenant's VM at three to eight
processors), 64 tokens, the report's prompt, tokens a second:

| Path | Plain | Drafting k=1 |
| --- | ---: | ---: |
| EyesOff's engine and harness on the host (`bench-spec2`, official build) | 16.45, 15.92 | 19.51, 16.60 |
| The same loop in the KVM Box image over its Shield backend | 14.78 | 14.23, 15.21 |
| The same loop over the OpenCL route | 11.96 | 11.53, 11.67 |
| llama-cli, native Shield, ring, 8 threads, 16 refill threads | 15.73 | |
| llama-cli, OpenCL route, same | 11.8 to 12.6 | |
| llama-cli, OpenCL route, TCP, same threads | 10.61 | |
| llama-cli, OpenCL route, as measured before (TCP, 2 threads, 4 refill threads, 8 processors) | 6.3 | |

So the settings are worth a factor of two to the route, and with them it is
at 75 to 80% of the native engine and about 74% of EyesOff's plain decode on
the same day. The drafting loop runs over the route unchanged (same accepted
drafts, identical text); it gains nothing in the image on either path today.

To run EyesOff's loop over the route: the engine shim and `bench-spec2` built
against this tree's llama, the OpenCL backend loaded as the device, the
recurrent state and KV kept off the device and fused attention on (what
`--no-kv-offload` and `--flash-attn on` do for llama-cli), and the executor's
shared regions large enough for a batch of full-vocabulary rows. The regions
are now 64 MiB and a request may have 64 rows (they were 2 MiB and 32; a
17-row prompt asked for 16.9 MB of logits).

The service takes the settings from its configuration (`link`: `workers`
with an optional `ring` each, `refillThreads`, `maxRows`, `poolDepth`,
`refillBatch`, `splitColumns`, `overlapVerify`), checked value by value.
Without it the defaults are unchanged.

Where the route's remaining 17 ms a token goes, from timestamps in the shared
region and the two timing reports (generation only, 261 requests a token):
about 7 ms in the two crossings between the processes (12 to 20 us back, the
same order forward; spinning brings the way back to 5 us and makes the whole
slower on a busy host), 4 ms in the frontend around its 401 launches, 2 ms in
the executor around the backend call, 2 ms in the scheduler's buffer reads
and writes, the rest in the client. A request for a product the executor
already started (the down projection) waits for it, about 190 us: that is the
product, not a cost of asking. Not established: any of this in a guest.

**What bounds a token, and a chain that did not shorten it.** A layer of this
model makes four exchanges, and each needs the one before: the opening
products are of norm(h); the next is of something only the client computes
(the recurrence or the attention); the feed-forward pair is of norm(h + that
product); the down projection is of their gating; and the next layer opens on
h + product + down projection. With 64 layers and about 175 us an exchange
that is 45 ms a token before any arithmetic, for the native engine as for the
route (the native engine: 1,000 us a layer, 280 of them on the processor).

An experiment (branch `kvm-box/shield-residual-chain`, `SHIELD_Q8_RESIDUAL=1`)
had the executor follow h itself: it finds the token's embedding row from the
first layer's request (by the signs of its first 64 values, then the whole
row), adds each product as the client does, applies the same norm, and so
computes the pair, the down projection and the next layer's opening products
before they are asked for, releasing each only to a request with the same
bytes. Every one of the 41,184 predicted requests in a 200-token run was
answered from the chain, none was computed in vain, and the text is the same.
The speed did not change (12.1 to 12.3 against 12.35): the three products are
540 us of the chain's 670, and they cannot overlap one another. What the route
adds to the native engine's layer is about 300 us in small pieces on that
path: the crossings, the frontend's launches, the scheduler's buffer copies,
and the client's part between the opening products and its next request,
which takes about 450 us here.

## A session without OpenCL, and without the kernel in between

Experiment, opt-in, not released. Three pieces, measured together on EyesOff's
27B under EyesOff's own loop (`bench-spec2`, 64 tokens, ring workers, four
engine threads, 16 refill threads), tokens a second:

| Path | Plain | Drafting k=1 |
| --- | ---: | ---: |
| OpenCL route | 12.33, 12.73 | 12.51, 12.74 |
| Session backend, socket | 13.08, 12.73, 12.84 | 13.68, 13.28, 13.56 |
| Session backend, mailbox | 13.75 | 13.04 |
| Session backend, mailbox, the two threads on one core | 14.78, 16.09, 15.40, 14.79, 15.51, 15.37 | 17.04, 16.48, 15.01, 16.00, 15.26, 16.41 |
| Native Shield engine in the image | 15.46, 15.94, 15.27 | 17.27, 16.63, 15.51 |
| EyesOff's engine on the host, same day | 16.45, 15.92 | 19.51, 16.60 |

The text is identical in every run. With all three the medians are 15.4 plain
and 16.2 drafting against the native engine's 15.5 and 16.6.

- **The session backend** (`native/ggml_shield_session.cpp`) is a ggml
  accelerator on host memory, as the native Shield backend is: it takes the
  weight x activation products and leaves everything else to the CPU backend.
  It sends them over the same session the OpenCL frontend uses, so the
  application still holds no worker route and no pads. It names a tensor by its
  GGUF name and the SHA-256 of the mapped bytes, computed once when the
  scheduler first asks about it; two products of one activation go in one
  request. Nothing is recognised, lowered or copied into device buffers: the
  weights stay mapped from the file. Only for applications built on ggml.
- **The mailbox** (`SHIELD_Q8_MAILBOX=1` for the executor,
  `SHIELD_SESSION_MAILBOX=1` for the backend) is a request slot in the last
  bytes of the session's shared memory. The client writes the request and adds
  one to a counter, the executor answers in place and sets its counter equal,
  and neither enters the kernel. The executor watches the slot for 2 ms after a
  request (`SHIELD_Q8_MAILBOX_SPIN_US`) and then sleeps in poll with a flag
  raised; a client that sees the flag sends a frame to wake it. What is asked
  and answered is a shared-memory product, checked as one.
- **One core for the two threads** (`SHIELD_SESSION_CPU`, `SHIELD_Q8_SERVE_CPU`,
  set to the two hardware threads of a core). The client's thread and the
  executor's serving thread never compute at the same time, so they can wait
  for each other on one core without taking a processor from anything else,
  and the activation stays in that core's cache. Spinning without this lost
  on a busy host (above); with it the crossing costs almost nothing. The
  executor pins itself after its first mailed product, when the backend's
  threads exist, because a thread made later inherits the one processor.

The mailbox and the one core are in the OpenCL frontend too
(`SHIELD_OPENCL_Q8_MAILBOX=1`, `SHIELD_OPENCL_Q8_CPU`), so the emulated device
has them for any application. The same loop over the OpenCL route, three
alternating rounds: 14.45, 14.79, 13.44 plain and 15.43, 15.33, 15.50 drafting
with them, against 12.54, 12.87, 12.89 and 12.77, 12.26, 12.67 over the socket.
That is 93% of the native engine; what is left is the OpenCL layer itself,
which the session backend does not have.

**No product ahead for a mailed request.** The executor's second thread, which
computes the down projection while the client gates the pair, loses here: the
client is waiting on the serving thread's own core, and a product made on
another thread comes back through a sleep and a cold cache. With it on, the
OpenCL route did 13.2 drafting and the session backend 13.6 to 16.4; without,
three more alternating rounds gave the OpenCL route 15.46, 14.30, 14.45 plain
and 13.53, 15.75, 15.20 drafting, and the session backend 15.20, 16.40, 16.49
and 16.86, 17.16, 17.02. Requests over the socket keep the second thread.

**Buffer reads and writes made directly** (`SHIELD_OPENCL_DIRECT_BUFFERS=1`).
A blocking read or write of a buffer, and the frontend's own two around each
product, copy from or to the CPU runtime's storage, mapped once for each
buffer, after the queue has been finished as before. Worth about 2% on the
OpenCL route (14.44, 14.65, 14.88 plain against 14.62, 14.04, 14.33 in
alternating rounds; drafting within the noise): the runtime's bookkeeping was
the smaller part of those calls, and the copy itself, 40 to 70 KB each way,
is how an OpenCL application gets a device's results. Only for a runtime
whose map is its storage, and it does not see reads and writes made through
entry points this library does not wrap.

Where that leaves the emulated device on this model: about 14.5 tokens a
second plain against the native engine's 15.9 that day, 92%. What remains is
the device model itself: about 7 us of launch handling for each of 401 kernel
launches a token, 18 kernel arguments each, and the copies in and out of
device buffers. The session backend has none of them.

**Three products in one exchange** (`SHIELD_OPENCL_Q8_TRIPLE=1` for the
frontend; the session backend always). Attention's q, k and v take one
activation; as a pair and a single they were two exchanges. The executor
builds one graph for the three (`Engine::Triple`, a fourth shared region, the
request's reserved word names the third slot plus one) and the frontend
learns the third as the tensor that follows a paired product just served. On
the OpenCL route, alternating rounds: 13.92, 14.67, 15.19 plain against 14.41,
14.03, 14.22, and 13.85, 14.21, 13.85 drafting against 14.59, 14.72, 14.58. A
few percent for one row, a loss for two; left opt-in. A spinning hand-off for
the executor's second thread on a core of its own was also tried and removed
(14.56, 14.22, 14.38 against 15.09, 14.17, 15.08 without the second thread).

**Drafting on the OpenCL route** stays near its plain speed (about 14.6)
while the native engine gains (17.2 to 19.5 that day) and the session backend
with it (17.0): a two-row round doubles every copy in and out of device
buffers, and the round costs the route 1.69 of its token against 1.5 for the
native engine.

**Which core.** The two threads cannot leave their core when something else is
scheduled on it, so the choice matters on a shared host. On a core whose
hardware threads were 2 to 4% busy before the run, the OpenCL route (mailbox,
direct buffers, work buffers in memory, triples) measured 14.95, 15.19, 14.66
plain and 15.69, 15.84, 14.79 drafting; on the one used for the tables above,
9 and 20% busy, 14.88, 14.27, 14.42 and 14.16, 14.43, 13.49 in the same
alternating rounds. The native engine in those rounds: 16.14, 15.51, 16.00 and
17.16, 19.16, 16.18. Binding the client's other engine threads to cores of
their own made it slower (14.32, 13.90, 14.19 plain); they are left free.

**The continuation on the serving thread** (`SHIELD_Q8_AHEAD_INLINE=1`). For a
mailed pair whose continuation is known, the serving thread computes the down
projection itself as soon as it has answered the pair, while the client gates
the pair on the core's other hardware thread: no hand-off, nothing leaves the
cache. OpenCL route on the quiet core, alternating rounds: 14.81, 14.62, 14.75
plain and 14.48, 15.55, 16.27 drafting with it, 14.55, 14.90, 14.19 and 14.95,
14.07, 14.59 without; the native engine 15.26, 15.71, 15.19 and 17.61, 16.78,
17.42. A little, and more with two rows.

**Where a token's time is on the OpenCL route**, from timers in the frontend
(llama-cli, one run each, milliseconds a token): over the socket 77.8 = 51.0
waiting on the executor + 3.7 in the frontend's launches + 23.0 outside them;
mailbox with the threads free 76.3 = 45.8 + 3.9 + 26.6; mailbox with the two
on one core 71.2 = 44.7 + 2.8 + 23.8; on two cores 67.7 = 41.0 + 3.0 + 23.8.
A mailed request reaches the executor in 0.3 to 0.9 us and its answer comes
back in 0.1 to 0.3. So the hand-off is no longer a cost, and the 23 ms
outside the launches does not depend on it: that is the application's own
path (its processor work, its copies in and out of device buffers, 18 kernel
arguments a launch, and with llama-cli its sampling over the vocabulary).
Under EyesOff's loop, which samples by a plain maximum, one core was better
than two in alternating rounds (15.02, 16.53, 14.92 against 14.71, 14.12,
14.35 plain).

Tried on that path and not kept: waiting with MONITORX/MWAITX in place of a
spinning loop, so that the waiting hardware thread leaves the core to the
other (14.59, 14.48, 14.55 against 14.75, 14.38, 14.58; a wait does sleep, 54
us for a 50 us limit); using the activation where it lies in the runtime's
storage with one copy into the shared memory (14.96 against 15.40, medians of
three); the residual chain run on the serving thread itself, a step the
client is waiting for computed straight into the shared memory (branch
`kvm-box/shield-residual-inline`: every prediction right, 14.64 against 14.90,
the chain's products 178 us each beside a busy sibling where one alone takes
150 to 160).

Not done: the backend aborts if the service refuses a product in the middle of
a run or says goodbye (the frontend reconnects and falls back); the executor
takes the processor number from its environment, which the service would have
to choose; the drafting loop needed fused attention on for the native engine
too (`ENCLAVE_GGML_FLASH_ATTN_ON` in the harness's copy of the shim), without
which both paths gained nothing from drafting; nothing ran in a guest. On the
OpenCL route the mailbox and the pinning are not wired in.

## Application buffers backed by a file

On the 27B the route held the model three times: the executor's backend copy
(as the native engine has), and every weight again in the private memory of
the buffers llama's OpenCL backend creates, 16 GB that is read only for a
product computed in the application. With two test workers a run needed more
than 50 GB where the native engine needs 31.

With `SHIELD_OPENCL_BUFFER_DIR` set, `clCreateBuffer` backs a buffer of 64 MB
or more (`SHIELD_OPENCL_BUFFER_FILE_MIN_MB`) that the application creates
without host memory by an unlinked file in that directory, handed to the
runtime as its host pointer. The pages are file pages: written back and
dropped under pressure, read again when wanted. The application is not told:
`clGetMemObjectInfo` reports such a buffer, and its sub-buffers, without a
host pointer, so it still changes only through calls this library sees and an
authenticated identity is remembered as before. Off unless the directory is
set.

Measured: the application's private memory falls from 1.94 GiB to 0.17 GiB on
the 3B model with the same text, exchanges and speed (45 to 47 tokens a second
both ways); the 27B runs above kept 18 GB or more available on a host with 57.
A buffer that is rewritten all the time is the wrong thing to put there:
llama's compute buffer (495 MiB on the 27B) qualifies by size, and with it in
memory instead (`SHIELD_OPENCL_BUFFER_FILE_MIN_MB=1000`; which of the weight
buffers that still leaves on the file was not checked) the route measured 14.40,
14.89, 14.82 tokens a second against 14.48, 14.56, 14.35. Size does not tell
weights from work space; that needs a better rule before this is a default.
The directory must be on a disk with room for the model. Not tried: a guest,
and a directory in memory, where it would save nothing.

## What is not established

- Nothing here ran in a measured image or a confidential VM, on NucBox, or
  through the native profile's launcher and broker. The two-processor numbers
  come from a container pinned to two cores, not from the guest.
- On the two small models the route is 6 to 8% behind the native path at the
  service's settings and 16 to 21% behind with four refill threads (re-measured
  after the changes above: 0.5B 92%, 3B Q8_0 94% and 84%, 3B Q4_K_M 94% and
  79%). There a token is a hundred and more small products, and the fixed
  cost of a round trip is a larger share of each. A whole run is shorter than
  the native engine's only when the executor is already running.
- Two models, both Qwen2.5 Q8_0. The 3B model has one feed-forward tensor the
  calibration refuses (`blk.2.ffn_down.weight`); both paths compute it on the
  CPU.
- On EyesOff's own model the route is level with the native engine when each
  card has a refill thread, and behind it (76 to 84%) with one refill thread
  for two cards, for the placement reason given above. The executor still
  registers approved tensors the application never uses.
- The rebuilt plugin, libllama and declines table exist only outside this
  repository; no image contains them. `test_q8_raw_kernels.cpp` executes the
  three plain kernels on the CPU runtime against the decoded weights (942
  outputs, both option sets); it was built and run by hand, not by a script
  in this repository.
- There is no kernel fixture,
  identity test or failure-injection test for K-quant launches; the Q8 ones
  pass unchanged, and the evidence for K-quants is the recogniser and
  admission tests plus the end-to-end run whose text and exchange counts equal
  the native engine's.
- The Q6_K local product has only been exercised by that run (one tensor).
- The continuation is learned only for a pair gated with `swiglu` and only if
  the executor's ggml produces the same bytes as the application's. It did for
  the packaged llama; another build, or another activation function, simply
  never learns it and runs as before.
- The passive OpenMP policy changes the native engine's timing too. It was
  measured on these two models only.
- Deferred builds were exercised with the packaged llama (identical text, five
  programs built instead of 119) and once with `sd-cli` (same image). They are
  on only for `llama-cli`. There is no unit test for the handle bookkeeping.
- The executor now needs `-pthread` when it is linked.
- Thirteen kernels are lowered (Q8_0, Q4_K, Q5_K, Q6_K and IQ4_NL, matrix and
  vector each; one plain kernel each for IQ4_XS, Q3_K and IQ3_S). Other
  kernels, other weight types and CUDA binaries are unchanged.
- The new code has had no independent security review.
