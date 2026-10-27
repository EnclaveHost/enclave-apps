# Shield Compute compiler experiment

This directory contains the original narrow graph compiler and execution
adapter, plus its later native-engine, OpenCL and CUDA/HIP source integration.
It reuses Enclave Shield's trusted C implementation for one-time masks, pad
refill and verified products. It adds no cryptographic construction. The v6.2
development desktop includes the protected Unix service and compatibility
toolchain; it is not a production security release or a complete CUDA driver.
See `IMPLEMENTATION.md`, `native/README.md`, `opencl/README.md` and
`hip/README.md` for current integration evidence and outstanding acceptance.

The intended architecture is:

    Application GPU API / GGML graph
        -> trusted frontend and typed graph inside the confidential VM
        -> compiler with explicit secrecy, numeric and shape contracts
        -> masked FIELD_GEMM to an untrusted GPU, or local CPU execution
        -> verification and unmasking inside the VM
        -> application result

There is no raw CUDA launch, generic GPU upload, device passthrough, arbitrary
RPC, or plaintext fallback in this adapter. This is an API boundary, not an OS
sandbox for a malicious application that can open its own network connections.
Production device mediation must enforce the boundary at the VM OS/broker as
well as inside the compiler. Host processes and a normal container do not
provide the confidential-VM guarantee.

## Implemented

`compiler.py` accepts a static `shield-graph/1` graph. Supported local operations
are matrix multiply, elementwise add/multiply, ReLU and matrix transpose. The
numeric contract is exact bounded signed integers; floating-point inputs and
implicit quantization are refused. Interval propagation rejects possible
int64 overflow. Unsupported kernels are rejected, not forwarded.

Inputs and constants are private by default. The trusted caller supplies the
typed SHA-256 digests of constants it explicitly approves for public disclosure.
Graph labels and tensor names cannot declare runtime data public. Constants are
copied into the compiled plan, so later source mutation cannot change them.

Only a matrix product with an approved constant right operand, weights in
[-119,119], supported geometry and a static field no-wrap bound is lowered to
Shield FIELD_GEMM. Other supported operations execute locally. Placement is
chosen from public contracts, not from the values of private inputs. Inputs
outside their declared ranges are rejected before any weight upload. Private
weights, attention products and nonlinear operations never enter the channel.

The graph backend accepts up to 4,096 rows for an eligible matrix product,
split into at most 64 requests of at most 64 rows on one registered channel.
All input bounds are checked before registration. Tile geometry depends only
on the declared shape; each tile uses the existing fresh-mask and verification
path. The full graph result is withheld until all tiles succeed, and staged
matrix rows are cleared on a later-tile failure. Service memory, work and
output limits still apply. The [graphics driver](../shield-graphics/driver/README.md) separately extends
its proven affine dispatch geometry to the same bound; this does not authorize
arbitrary shaders or floating-point quantization.

`channel.py` binds the existing `sh_link_*` API with verification always enabled.
It independently checks public-weight digests against its trusted caller policy
before registration, even when invoked without the graph compiler.
It retains private weight copies for the C library's refill lifetime, uses
private result scratch, serializes operations, and retires the channel on any
transport or verification failure. No graph outputs are returned on failure.
There is no retry using an unprotected GPU. An offline CPU run is explicitly
selected before execution by omitting the channel.

After a graph channel failure, service status includes `graphFailure` with a
fixed stage (`initialize`, `register`, `start`, `compute`, or `unknown`) and a
bounded numeric C error code when available. It never contains arbitrary
exception messages, worker strings or tensors. This is diagnostic only:
retirement, output withholding and rejection of subsequent work are unchanged.

The October 27 candidate passed dedicated GPU transport tests for 65, 129 and
4,096 rows, exact reference outputs, distinct masks for repeated input tiles,
and corruption in the second and final tile. The production desktop's older
compiler has not been replaced with this candidate. A live adapter test that
opened a separate graph connection for each tile retired that older service;
the matching 63-connection cutoff was reproduced in the Windows bridge, whose
completed-stream cleanup fix is maintained in the host repository. See the
deployment evidence before treating candidate tests as live acceptance.

The new Python code is experimental and has not had an independent security
review. The model's declared ranges and exact integer arithmetic are intentional
limitations, not a claim that arbitrary CUDA floating-point semantics are met.

## Run

Requires Python 3.10+, NumPy and a C toolchain. Build in an isolated output
directory using `build-core.py --enclave ENCLAVE_SOURCE --output BUILD_DIR`.
The recipe copies and hashes source and builds without `SHIELDED_DEALER_MODE`.
It never changes the existing Shield checkout or production workers.

Run `python3 -m unittest -v test_compiler` in this directory. A dedicated worker
is required for `python3 smoke.py --library BUILD_DIR/libshield-core.so --port PORT`.
The smoke test uses synthetic data, confirms nonzero remote exchanges, compares
against an exact CPU reference, and reports which operations stayed local.
Also run the built `postmortem-selftest` for value-free failure diagnostics
and `shielded-probe --host HOST --port PORT` for protocol rejection,
corruption detection and exact masked execution.

Set `SHIELD_COMPUTE_TEST_LIBRARY` and `SHIELD_COMPUTE_TEST_PORT` and run
`python3 -m unittest -v test_transport` for real GPU tests through a local
fault-injecting proxy. These check distinct masks for repeated plaintext and
verify that a changed GPU reply retires the channel without releasing output.
The underlying C retirement regression is Enclave's
`node --test test/shielded-overlap-verify.test.mjs`.

No `SHIELDED_*` environment variables are accepted by this adapter. Use a fresh
dedicated process. Do not load a dealer build or an arbitrary untrusted library.
For deployment, pin the library, compiler, policy and CPU implementation into
the measured VM image and authenticate the policy before releasing private data.

## GPU compatibility work still required

llama.cpp and sd.cpp already expose GGML graphs, and Enclave has a Shield GGML
backend plus an sd.cpp accelerator scheduler. These are integrated through the
native adapters described in `native/README.md`; the standalone integer graph
prototype does not run either engine. Their calibrated
quantized arithmetic needs an explicit accuracy contract and authenticated
public model weights. A model file or calibration name alone is not evidence
that its weights are public or authentic.

A CUDA-compatible frontend must mediate contexts, allocations, copies, modules,
streams, events and library calls. Device pointers must refer to private guest
storage until a vetted lowering masks data. Capture cuBLAS operations or a
framework graph first; those retain matrix semantics. PTX/source compilation
can extend coverage with alias analysis, secrecy propagation, bounds and
verified rewrite rules. A context alone contains insufficient semantics.
An opaque cubin, unsupported intrinsic, atomic, texture operation, secret-indexed
access or unknown kernel must have a faithful trusted CPU implementation or
return an unsupported-operation error. No fallback to native GPU execution.

Do not advertise unrestricted CUDA, graphics/Vulkan, floating-point bitwise
equivalence, or acceleration of every operation. Preserving IEEE rounding,
atomics and synchronization is part of correctness. Masking a float and later
subtracting the mask is not a substitute for Shield's exact field protocol.

Production acceptance must test native llama.cpp/sd.cpp end to end inside an
attested VM, inspect the guest's device and socket exposure, inject corrupt and
replayed replies, test exhaustion/concurrency, and demonstrate that no raw
tensor copies cross the boundary. It must distinguish API coverage, GPU-offload
coverage, model accuracy and sustained performance. Pad preparation still costs
trusted CPU work; universal GPU speedup is not implied.

Approved weights, static graph metadata/shapes, traffic sizes and timing remain
visible to the GPU operator. This design does not promise access-pattern or
timing obliviousness, availability, or immunity to confidential-VM side channels.
