# Shield OpenCL development runtime

This is the standard-API frontend under development for KVM Box, installed in
the separately measured KVM Box development VM. The CUDA/HIP source layer in
`../hip/` builds on this runtime. It is not a full NVIDIA driver,
a physical GPU passthrough, or a completed/conformance-certified OpenCL
implementation. Do not publish it as
the finished virtual GPU. The full acceptance ledger is `../IMPLEMENTATION.md`.

## Execution model

Ordinary OpenCL host programs link/load `libOpenCL.so.1`. The frontend supplies
a logical GPU backed by the guest's PoCL CPU ICD and the guest-local Shield
service. It maps GPU discovery to that device and keeps PoCL's contexts,
allocations, buffers, program compilation, queues, events and other operations.
The packaged runtime must contain only the CPU ICD. The frontend is not an
isolation boundary: the VM's UID/firewall/seccomp policy and measured service
continue to enforce the only permitted physical-GPU path.

A Clang AST pass recognizes the full typed semantics of the integer matrix
kernel in `gemm_i8.cl`. Renaming variables/functions, comments, declaration
grouping and redundant parentheses do not define eligibility. Indexing, casts,
signedness, writes, calls, bounds and arithmetic remain in the comparison.
Changed or unknown programs retain PoCL's CPU compilation and execution. Raw
source is never sent to the remote GPU. This first pass recognizes one kernel
family, not arbitrary matrix algorithms or optimized tiled CUDA kernels.

The recognized signature is two global signed-int8 inputs, a global signed-int64
output, and uint32 M/N/K. The runtime also checks complete zero-offset 2D grids,
buffer sizes/contexts, output-storage disjointness (including sub-buffers), local
workgroup limits and resource bounds. It sends a typed graph and private inputs
over the guest Unix socket. The measured service independently authenticates
public B by its typed content digest. Unapproved B stays local. Approved B can
use the existing masked FIELD_GEMM only when its static integer bounds prove no
field wrap. There is no implicit floating-point quantization.

Inputs are not copied until their OpenCL dependencies complete. Submission
returns while an unsignaled user event is pending. A private queue handles
transfers and a completion event orders subsequent commands on the application's
queue. The output is copied back only after a successful service response.
Service/integrity/transport failures fail the event and do not retry a raw GPU
path or publish partial output. Only an explicit admission-busy response is
retryable, for at most 20 minutes. A process admits at most 16 pending offloads.

Profiling-enabled queues can now use protected offload with logical command
timings in the backing CPU device's clock domain; see the v6.2 section below.
Opaque binary programs, additional build options,
unsupported geometries and unsupported kernels also remain on the CPU path.
Observed SPIR-V programs may now qualify through the structural LLVM recognizer
described in `../hip/README.md`. This is separate from source AST recognition;
unknown IL, specialization constants and unobserved link inputs remain local.
CPU-only execution still runs within the confidential VM after deployment; a
host test container does not provide that confidentiality.

## Build and test

```sh
docker build -t enclave-kvm-shield-opencl-builder:1 .
docker run --rm --memory=3g --cpus=1 -v "$PWD:/src" \
  enclave-kvm-shield-opencl-builder:1 \
  make all test-opencl 'CPPFLAGS=-DSHIELD_COMPILER=\"/src/kernel_compiler.py\"'
docker run --rm -v "$PWD:/src" enclave-kvm-shield-opencl-builder:1 \
  python3 -m unittest -v test_kernel_compiler
```

The result under `runtime/` contains the frontend and the standard ocl-icd loader
with a distinct SONAME; the frontend depends on that loader and delegates
remaining API symbols. GNU OpenCL symbol versions are retained. For host testing,
set `LD_LIBRARY_PATH=/src/runtime`; no LD_PRELOAD is required. Deployment must
build with the installed compiler path and configure normal loader discovery.

`test_service_runner.py` is a host integration harness. It expects the source at
`/shield-code`, trusted core library at `/core/libshield-core.so`, and a dedicated
worker at 127.0.0.1:9741. It creates a private Unix service inside the test
container, approves only a deterministic public fixture, and checks actual
service routes/counters rather than inferring offload from a device name.

Seven compiler tests cover acceptance under harmless source edits and rejection
under changed bounds, indexing, casts, signedness, extra writes, helper functions
and build flags, including a source redeclaration that redirects an indexing
builtin through an assembly label. Twelve integration modes passed with the ordinary library-loading
path: masked execution, private-weight CPU execution, service failure, busy
recovery, failed dependency, malformed response, oversized integer output,
sub-buffers, cloned kernels, out-of-order queues, profiling and binary programs.
The binary test verifies zero service requests. Profiling originally used the
CPU path; the v6.2 test now requires a verified masked exchange. Every mode runs a float/sin
kernel through PoCL and compares its result. Successful GEMM is compared exactly
against a CPU reference, uses a padded grid, and waits on an initially unsignaled
user event. The masked fixture recorded one verified exchange, 2,048 multiply-
accumulates and zero verification failures. Failure modes check that the output
sentinel is unchanged; the failed-dependency test also proves no service request
was made. Sub-buffer tests also verify that surrounding bytes are untouched.

## CPU runtime and image packaging

The Debian PoCL 6.0 package fails the Khronos tests for callbacks on failed
events. `pocl-callback-errors.patch` fixes completion callbacks for negative
statuses, passes the actual error code, and retains user events while callbacks
can consume the application's last reference. `test_event_errors.cpp` tests
both early/late registration and release inside the callback. This patch is
applied only to a private export of PoCL v6.0, commit
`952bc559f790e5deb5ae48692c4a19619b53fcdc`.

Build `pocl-build.Dockerfile` as `enclave-kvm-shield-pocl-builder:1`, then run
`build-pocl.sh` in that container with the pristine source export mounted at
`/input`, this directory at `/frontend`, and an empty private build directory at
`/build`. The package is produced under `/build/package`. Only basic/pthread
CPU drivers are enabled; CUDA, Vulkan, HSA and remote drivers are disabled.
LLVM/Clang 18 and a generic x86-64 kernel library are used. The package records
build dependency versions and includes the upstream license. Do not reuse a
package made from a different source revision or omit the callback patch.

`prepare-desktop.py --pocl-package BUILD/package --output NEW_CONTEXT` assembles
a separate Docker context based on the native 0.4.0 desktop. Its Dockerfile
builds the installed frontend path, installs the CPU runtime, configures normal
system loader discovery, and adds only the deterministic public self-test
matrix to the measured service policy. `inputs.json` records input file hashes,
the base image ID, PoCL source revision and patch hash. Pass the resulting OCI
image to `host/build.py` with `--version 0.5.0`, the existing authenticated model
volume and a new work/release directory. Never overwrite a running release.

The installed commands are `clinfo -l`, `shield-opencl-selftest`, and
`shield-opencl-event-test`. `shield-compute status` reports service-owned
cumulative verified offload/MAC counters and the last completed job without
including prompts or tensor contents. It reports activity, not physical GPU
utilization, VRAM residency, or an nvtop/NVML device.

Targeted Khronos OpenCL-CTS validation at commit
`16b197d191903abad0047e6bcb113167b589ecb4` passes 16 API tests, all 30 event tests,
and 13 buffer tests. The packaged image also passes the 16 API and 13 buffer
tests and a real masked launch without LD_PRELOAD or LD_LIBRARY_PATH. These
are selected compatibility checks, not full conformance certification. The
frontend deliberately clears the underlying runtime's conformance-version
claim because it cannot certify this modified device.

## Measured VM evidence

The v5 desktop and Wasm controller booted through their SNP verification gates.
Inside the guest, stock `clinfo -l` discovers the logical device without loader
environment overrides. `shield-opencl-selftest` produced an exact integer result
and a float/transcendental CPU result within tolerance; the measured service
reported one verified exchange and 2,048 masked multiply-accumulates. The
guest boundary check still reports no raw GPU devices, direct-worker access
denied, VSOCK/io_uring denied and privilege escalation disabled. This is actual
guest execution evidence, separate from the host container tests above.

Native Qwen and SD masked and CPU reference smoke tests also completed in v5.
Qwen returned Paris with 726 verified offloads; SD recorded 32 offloads and 288
local operations. Both reported zero verification failures. These small jobs
were slower than their CPU references: 4.15 vs 0.728 seconds for Qwen and 87.625
vs 24.182 seconds for SD. The SD outputs visibly differ (MAE 20.30/255,
PSNR 18.40 dB for one fixed prompt/seed). This is not a demonstrated speedup or
validated general image-quality result. Approximation quality and performance
remain acceptance work.

Broader API, concurrency, numerical and application validation;
CUDA/HIP frontend integration; monitoring; and the deployed fault
suite remain required before completion.

## SVM development after v5

The next frontend build tracks `clSVMAlloc`, direct and queued frees, and
`clSetKernelArgSVMPointer`. It validates allocation context, available byte
ranges and output overlap before lowering an all-SVM launch, and retains
allocations for its pending job. Input/output transfers use ordered SVM copy
commands. Mixed buffer/SVM arguments remain on PoCL. Rebinding either argument
kind updates the captured launch state; stale cl_mem handles can no longer be
used after setting SVM pointers. Three-dimensional grids are eligible only
when the third dimension is exactly one, preserving the recognized 2D kernel.

Ten SVM host integration modes pass: masked, subrange, rebind, mixed, queued
free, private weights, failed service/dependency, malformed response and integer
overflow. Positive controls record one verified GPU exchange and exact output;
failure controls preserve output. Eight targeted Khronos SVM cases pass for
allocation, negative allocation, enqueue APIs, migration, overlapping-copy
rejection, pointer passing, execution-info pointers and shared sub-buffers.
All twelve pre-existing OpenCL integration modes still pass. These additions
were subsequently installed in v6. The separately built CUDA/HIP
source compiler is documented in `../hip/README.md`.

## Program lifetime and concurrent build provenance (v6.1)

Program, kernel and event metadata is retired before calling the underlying
driver's final release, so a recycled native handle cannot inherit or lose
another object's metadata. Each program build/compile has a fresh revision.
Compiler results are published only for the matching revision; linking also
rechecks the revisions of compiled inputs after the driver returns. A changed
input or stale analysis conservatively loses masked eligibility.

`test_program_lifetime.py` compiles the production frontend against a private
deterministic driver/compiler fixture. Seven cases cover recycled source/IL
handles, retained programs, late build/compile analysis, a recompiled link input,
and an unchanged positive link. The fixture deliberately interleaves operations
and reuses native addresses. It tests metadata authorization lifetime, not the
real compiler's semantic analysis or GPU confidentiality. The seven cases pass;
all 22 OpenCL/SVM integration cases and the selected 16 API, 30 event and eight
SVM CTS cases also pass with this runtime. These are targeted regression results,
not a complete concurrency or OpenCL conformance claim.

## Logical command profiling (v6.2)

Recognized kernels on profiling queues now use the same protected route as
non-profiling queues. Timestamps come from `clGetDeviceAndHostTimer` on the
backing CPU ICD, which shares its timebase with ordinary CPU kernel/copy events.
If the backing device does not support synchronized timers, the command retains
its CPU path. No physical GPU timestamps are accepted from the worker.

`clGetEventProfilingInfo` reports queue, submission, execution start, execution
end and completion timestamps. Execution begins after dependencies complete and
includes input transfers, serialization, service admission, masking, protected
computation, verification and output delivery. End and complete are equal for
these commands, which do not enqueue device-side child kernels. These timings
describe the logical device, not CUDA kernel duration on the physical GPU.
Pending, failed and non-profiling events return profiling-info-not-available.
Ordinary CPU events retain their original runtime behavior.

Completion callbacks hold a frontend reference until they return, so an early
caller release does not discard command type or profiling metadata. A callback
may retain the event for later use or consume the caller's reference. Nine
targeted host cases pass: protected profiling, private-weight local execution,
service failure, dependency failure, out-of-order execution, early release,
early release on failure, late callback release and retain inside a callback.
Tests include exact outputs, unchanged failed output, a known service delay,
queue timestamp bounds, dependency timing, ordered timestamps, CPU-clock
continuity, invalid queries and query availability. Each positive protected
fixture records a verified GPU exchange; clock readings alone do not prove it.

These tests follow the [Khronos event profiling contract](https://registry.khronos.org/OpenCL/specs/unified/refpages/man/html/clGetEventProfilingInfo.html).
The complete device remains under development; this is targeted profiling and
callback validation, not full OpenCL or CUDA conformance.
