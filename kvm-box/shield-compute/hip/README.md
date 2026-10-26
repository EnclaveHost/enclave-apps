# CUDA/HIP source compatibility development

This toolchain compiles selected ordinary CUDA and HIP source programs into
SPIR-V, then runs them through Shield's logical OpenCL device. It is host-tested
development work, now installed in the separately measured v6.2 VM. It does not load
NVIDIA cubins/PTX or provide complete CUDA, HIP, cuBLAS, cuDNN, or nvtop support.

**The initial compiled integer GEMM family now supports masked offload.** A
structural LLVM pass compares the linked SPIR-V semantics with an audited
reference kernel, after bounded helper inlining and normalization. It rejects
unknown imports, mutable module state, specialization constants and changed
math, indexing or memory effects. Eligibility is bound to the actual OpenCL
program's observed IL/compile/link lifecycle. Kernel names and metadata do not
authorize offload. Public-weight policy and runtime bounds/alias checks still
apply; other kernels execute through the CPU ICD. A device name, compilation, or a
sample's printed word “GPU” is not proof of physical GPU execution. Only the
trusted service's routes and verified exchange counters establish that.

## Pinned inputs and build

chipStar is pinned to v1.2.1 commit
`3bd6515ebe2a84f2f6d17bee53ed14a5d19a0856`. `prepare-source.py` exports Git objects
at that exact revision and five pinned submodule revisions. Checkout edits are
not included. `shield-source.json` records revisions and archive hashes. The
build uses Clang/LLVM 18, the matching SPIR-V translator, and only OpenCL; Level
Zero and Intel USM are absent from this builder. Unsupported HIP functions use
the upstream error behavior rather than enabling another GPU backend.

The patch fixes glibc 2.41 compilation: SPIR-V cannot represent binary128, and
new pi-scaled math declarations require matching exception/linkage signatures.
Testing also exposed an upstream host-math bug: its fraction helper returned
the integer part, giving `sinpif(-2.5) == -0`. The patch uses floor-based fractional
reduction; the host and device test now agrees with the reference for seven
positive/negative integer and half-integer inputs. This is a targeted regression
check, not complete validation of chipStar's math library.

The patch also propagates negative OpenCL event status through HIP stream query
and synchronization, and tolerates an already-reported stream failure during
process teardown. Stream destruction waits for its own queue rather than all
device queues. It disposes a failed queue while returning its launch error;
an independent healthy stream can still be destroyed. The failed stream remains
poisoned until destruction. Applications may create a fresh stream for CPU work;
masked work additionally requires a healthy protected service. An integrity or
transport failure retires that service until the administrator restarts the VM.
Creating a stream or clearing the runtime's last error cannot reset retirement.
The Shield device bypasses chipStar's opaque program-binary
cache so each process resubmits IL for analysis. PoCL's compilation cache remains
available. This cache policy does not itself authorize any offload.

Build the OpenCL/PoCL builders first, then:

```sh
docker build -t enclave-kvm-shield-hip-builder:1 .
python3 prepare-source.py --checkout /path/to/chipStar --output /path/to/new-source
mkdir /path/to/new-build
docker run --rm --memory=3g --cpus=2 \
  -v /path/to/new-source:/input:ro \
  -v /path/to/new-build:/build \
  -v "$PWD:/frontend:ro" \
  enclave-kvm-shield-hip-builder:1 sh /frontend/build.sh
```

The checkout must have the pinned submodule objects available (`git submodule
update --init HIP HIPCC bitcode/ROCm-Device-Libs hip-tests hip-testsuite`). The
export and build directories must be new. The package is under
`new-build/package/opt/shield/hip`; it records source hashes, patch hash, package
versions and upstream licenses. The compiler expects LLVM 18 in
`/usr/lib/llvm-18`, with `llvm-spirv` linked to `/usr/bin/llvm-spirv-18` there.
These dependencies must accompany a future measured-image installation.

`build.sh` makes the generated SPIR-V extraction utility executable by normal
users and sets libCHIP's RUNPATH to the packaged Shield frontend. The package
records the build script hash as well as the source and patch. The separate
desktop image uses executable compiler wrappers because cucc's include-path
discovery does not follow a symlink correctly.

Build a separate desktop image from the package:

```sh
python3 prepare-desktop.py --hip-package /path/to/new-build/package \
  --output /path/to/new-desktop-context
docker build -t enclave-kvm-box-desktop:0.6.2-profiling /path/to/new-desktop-context
```

Inside the image, a normal user can compile the supplied CUDA source:

```sh
cucc /opt/shield/hip/share/examples/shield-gemm.cu -o /tmp/shield-gemm
/tmp/shield-gemm
```

The default run requires the protected local service; its deterministic public
fixture is approved by the measured test policy. In v6.2, adding `profiling`
enables CUDA event timing while preserving eligible protected offload. The
reported interval covers logical execution, including the service round trip,
and is not physical GPU kernel time. The prebuilt equivalent is
`shield-cuda-selftest`. Neither command's device label alone proves offload.

## Validation and execution scope

`test-host.sh` runs with the pinned source at `/input`, this directory at
`/frontend`, the installed toolchain at `/opt/shield/hip`, and writable `/tests`.
Configure `LD_LIBRARY_PATH` to the Shield OpenCL frontend and `OCL_ICD_VENDORS`
to the patched CPU-only PoCL ICD. Do not mount a physical GPU into this test.

The selected programs exercise source compilation, allocation, copies, kernel
launch, workgroup-local memory/barriers, streams, events and synchronization:

- Upstream HIP matrix transpose.
- Upstream CUDA vector addition (50,000 elements).
- Upstream CUDA tiled matrix multiplication with reference checking.
- Upstream CUDA asynchronous copies/launch/event polling with output checking.
- `test_cuda.cu`: exact signed-int8-to-int64 GEMM through CUDA runtime calls.
- `test_math.cpp`: host/device pi-scaled float and double arithmetic regression.

The upstream sample helper may print “Ampere” after warning that its SM lookup
does not recognize the compatibility value 2.0. That string is a sample-table
fallback and does not identify the backing hardware. These tests run locally
on PoCL, with the integer GEMM explicitly using a non-offloadable K=31 shape; no masked
offload is claimed for this six-program CPU compatibility suite.

Ten additional CUDA integration modes pass against the fresh packaged runtime:
masked, private, failure, busy, malformed, overflow, changed-kernel, repeat-process,
profiling and recovery. The positive fixture records one verified exchange and 2,048 MACs
with an exact output comparison. Both repeat processes offload, proving that a
cache hit does not silently lose eligibility. A changed kernel produces no
service requests. The v6.2 profiling case requires verified offload and checks
`cudaEventElapsedTime` across a known service delay. Failure modes require visible runtime errors
and unchanged output. Private weights remain local. The packaged desktop's
prebuilt CUDA test also records a verified masked exchange without LD_LIBRARY_PATH.
These are host-container results, not confidential-VM attestation evidence.

The recovery fixture first rejects a service job without retiring the service,
then destroys the failed stream and completes a verified masked GEMM on a fresh
stream. This specifically tests a recoverable job rejection. It does not grant
recovery after a cryptographic verification failure. The v6.1 deployed replay
test separately verifies that integrity retirement blocks subsequent submissions
while fresh-stream CPU operations continue to work.

The v6 guest also compiled this source with `cucc` as desktop uid 1000 and ran
the resulting native x86_64 executable. The first launch produced an exact result
and the guest service recorded one verified exchange / 2,048 MACs. A subsequent
process produced another verified exchange. The v6 controller and desktop passed
their predicted SNP measurements. Deployment-specific host_data is not checked
in this unpublished development setup. Boundary checks report no raw GPU device,
denied VSOCK/io_uring/direct-worker access, and an available protected Unix service.
In that historical v6 build, an isolated profiling-mode run returned the exact
result while leaving service job and offload counters unchanged. The v6.2
profiling implementation supersedes that CPU-only behavior.

The expanded compiler image exceeded the firmware's initrd allocation with
gzip. The bootable v6 release uses `host/build.py --compression xz` (535 MiB
instead of 747 MiB), verified against a guest kernel with `CONFIG_RD_XZ=y`.
The existing v5 release remains available for rollback. Moonlight pairing was
verified and its video/audio stream restored after the image change.

The underlying SVM frontend separately passes ten host integration modes,
including real verified offload, interior pointers with untouched padding,
SVM/buffer argument rebinding, mixed-argument CPU fallback, queued free, private
weight fallback, dependency/service failures and malformed/overflow replies.
Eight selected Khronos SVM tests also pass. The previous twelve OpenCL host
integration modes still pass after these changes. See `../opencl/README.md`.

## Remaining integration

Broaden the compiled SPIR-V/LLVM lowering beyond the initial exact integer GEMM
family, including real application kernels. Unknown kernels must remain local.
Broaden profiling validation beyond the current logical command/event tests.
Preserve public-weight
authentication, exact numeric contracts, bounds/alias checks, asynchronous
dependencies and fail-closed service errors. Validate real native workloads,
and extend the deployed fault suite beyond these successful guest smoke tests.

This does not complete the broader acceptance ledger in `../IMPLEMENTATION.md`.
