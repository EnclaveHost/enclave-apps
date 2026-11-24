# Packaged standard GPU applications

## 0.6.12 direct candidate (2026-11-23): built, gated in the image, not booted

`enclave-kvm-box-desktop:0.6.12-direct-candidate`
(`sha256:32cdbc6f1280fc27dbff8b2d1c28eab1408655dc8a762605392921ea5c8a7b56`)
holds the direct OpenCL route of `../opencl/Q8_DIRECT.md` as it is on `main`,
on the same base image as 0.6.11. It is a local development image: unsigned,
unpublished, in no catalog.

What is new in it against 0.6.11: the frontend (all of its `.inc` files, bound
to a regenerated table of 122 requests with 109 declines), the K-quant
compiler and its thirteen reference kernels, llama's OpenCL plugin and
libllama built by `build-llama-opencl.py` from the pinned source and the seven
bound patches, the executor, `service.py` and `q8_service.py`. Stable
Diffusion's library, the CLI library, the CPU runtime and its bitcode are the
0.6.11 binaries.

What the image turns on. The packaged `llama-cli` sets
`SHIELD_OPENCL_DIRECT_BUFFERS=1` and `SHIELD_OPENCL_LAZY_ARGS=1` beside the
settings it had; `service.json` gets `q8ExchangeEveryProduct: true`, so equal
requests give equal bytes and the exchange counts below are exact. Everything
else measured in `Q8_DIRECT.md` stays off; the reasons are under "What the
0.6.12 candidate sets" there. `sd-cli` is unchanged.

Passed:

- `python3 -m unittest test_source_declines_package` (the source lists
  against the tree) and the sixteen mutation tests against the real build.
- Image finalization: overlay and SDK digests, and every one of the 122
  requests classified twice with canaries before and after, at the 8 MiB and
  the 32 MiB file bound.
- `test_standard_package.py` on the installed image, three runs: normal
  (SD 392 verified exchanges, 51,921,289,216 MACs, the fixture PNG
  `048b2445…7abf80`; llama 487 exchanges, 14,273,495,040 MACs, the Paris
  answer; the native service and both loopback identities), a corrupted worker
  reply and a dropped worker connection (each fails closed, retires the
  service and refuses a retry).
- `../test_q8_session.py` in the installed image: 13 tests.

Not passed, and where it stopped: the measured images were built
(`host/build.py`, version `0.6.12-direct-candidate`, 8 GiB desktop) and
started with `host/start.sh --development-unbound`. The controller guest
booted under SNP and its attestation was rejected: "the runtime self-test
covered NO runtime process (runtime=0)". The front built from today's enclave
repository counts a process as the runtime only when it does not run as root,
and `guest/controller-start.sh` starts the runtime as root; 0.6.11 was
accepted in October because its older front stated no count and the verifier
took the legacy form. This is the controller, not Shield Compute, and the gate
was left as it is. The desktop guest was therefore never started: **nothing
of 0.6.12 has run in a guest**, no in-guest llama or SD check was made, and
the 0.6.11 guest figures below are the last guest evidence there is.
`test_q8_session.py` cannot run in the guest as written in any case: it
starts its own executor against a worker on loopback, and in the guest only
the service identity may reach the worker.

Two things the gates found in the route itself:

- Under the service's 8 MiB file bound the executor was killed (SIGXFSZ) when
  it sized the 64 MiB session regions, so every direct session failed and
  retired the service. The executor now ignores that signal, the regions are
  refused and the client keeps to the socket. In the packaged service there
  are therefore no shared regions, and so no mailbox and no arenas, whatever
  is switched on: the 97 to 98% configuration of `Q8_DIRECT.md` does not exist
  under this bound. The session tests do get regions, because they build the
  service in process without the bound.
- llama's check counts 487 exchanges on the direct route, as `Q8_DIRECT.md`
  recorded; the test still held the queued route's 511 and was corrected.

The commands and receipts of this build are outside the repository, in
`work/shield-compat/direct-0.6.12` and `work/kvm-box-v6.12-direct` of the
2026-10-23 working directory.

## 0.6.11 and before

The 0.6.11 compiler-rejection development image is running locally and has
passed measured-image guest validation. Standard CLI SD/Qwen recorded 392/511
verified exchanges, with the expected output and unchanged MAC counts; the
native SDK and guest boundary checks also pass. Its release gate verifies the
frontend, rejection table, compiler inputs and installed sandbox behavior.
HOST_DATA remains unchecked; this is not a production publication. The
following 0.6.9 figures are historical standard-path evidence.
Controller and desktop SNP gates/replay checks pass. Ordinary desktop UID 1000
ran both standard CLI applications: SD recorded 392 verified exchanges and an
image identical to the direct SDK fixture; llama recorded 511 and generated the
tested Paris response. Their VM times were 55.138s and 63.437s. These are short
compatibility checks, not broad quality or speedup acceptance.

The native service regression also passes: CPU/masked SD retain the previous
image hashes, and native llama completes verified work. Guest boundary checks
pass, logical nvtop counters update, and local Moonlight is restored at
1280x800/30 FPS with stereo audio. HOST_DATA remains unchecked development
binding, and the app is unpublished. The menu entries remain during acceptance.

The package installs ordinary `llama-cli` and `sd-cli` commands. They use a
separate application binary/backend directory, `/opt/shield/apps/bin`, so the
trusted service at `/opt/shield/bin` retains its existing native backend setup.
The wrappers select native CPU fallback for unsupported operations. Only
eligible, authenticated Q8 matmul may use the masked service.

Approximate arithmetic is explicit. Example commands inside the candidate VM:

```sh
SHIELD_OPENCL_ARITHMETIC=shield-fixed-point-v1 \
llama-cli -m /models/qwen2.5-0.5b-instruct-q8_0.gguf \
  --device GPUOpenCL -ngl 99 -p 'The capital of France is' -n 8 \
  -c 512 -b 32 -ub 32 -t 2 --temp 0 --seed 42 --flash-attn off \
  --no-conversation --single-turn --no-warmup

SHIELD_OPENCL_ARITHMETIC=shield-fixed-point-v1 \
sd-cli -m /models/sd15-q8.gguf --backend GPUOpenCL \
  -p 'A watercolor cabin beside a lake, pine trees, morning light' \
  -o /tmp/cabin.png --width 256 --height 256 --steps 2 \
  --cfg-scale 7 --sampling-method euler --seed 42 --threads 2 \
  --mmap --diffusion-fa
```

Without the arithmetic opt-in, work stays on trusted CPU. The llama wrapper
keeps its persistent KV cache on CPU; unsupported SET_ROWS cannot mutate an
OpenCL allocation. Numerical agreement with the original CPU model is not
promised by the approximate Shield contract. See `STANDARD_SD_MASKED.md`.

## Package boundary

The frontend invokes its installed compiler at
`/opt/shield/opencl/q8_capture_compiler.py`; it has no build-directory dependency.
The compiler and reference kernels are part of the measured image. The frontend
is compiled with both Q8 capture/masked execution and an empty diagnostic-log
path. This removes the development-only `/evidence` dependency, not verification
or service accounting. Test builds with a nonempty audit path still fail closed
if that log cannot be written.

Opaque GGML application binary caches cannot carry source authorization. The
Shield device therefore always submits source to the frontend for validation.
PoCL's internal CPU compiler cache remains available. The installed-image test
runs SD then llama under the same user/cache: both must record fresh verified
exchanges. The initial candidate exposed this issue by recording zero exchanges
for the second application; that candidate is superseded.

Ordinary desktop llama processes use 127.0.0.1. The measured service's effective
UID 1001 keeps 127.0.0.3, for both internal HTTP server and client. Guest nftables
rejects both destination and source traffic on that private address for other
UIDs. Blocking destination alone allowed another user to impersonate a server
by replying from 127.0.0.3. `test_private_loopback.py` reproduces that on the old
rules and verifies rejection on the new rules, while both legitimate paths
continue working. It requires a separate `--network none` container namespace;
it refuses to alter a namespace containing any interface other than loopback.

## Build and verification

`package-standard.py --help` lists the required immutable base and artifact
inputs, including `--source-declines-build` for the current release recipe.
It verifies the bound frontend/CPU-loader build, rechecks all captured source
classifications during image finalization, verifies the original SDK manifest, records artifact/source/patch
hashes, audits ELF dependencies and executable stacks, and stages an overlay.
`build-standard-image.py --context CONTEXT --tag TAG` builds locally using a
content-named base alias and verifies the output layer prefix against the
requested immutable base image. It never deploys or publishes. The build
verifies untouched SDK files, installs the administrator-owned Q8 setting,
preserves the base's attested-service port policy, and updates manifests.

The supplied libraries come from the pinned, patched native/GGML/PoCL builds
listed in `STANDARD_SD_MASKED.md`, plus `llama-desktop-loopback.patch` and
`llama-shield-source-cache.patch`. Since 0.6.12 the plugin and libllama are not
supplied as files: `build-llama-opencl.py --source LLAMA --builder-image
sha256:... --output BUILD` copies the tree `prepare.py` exported, applies the
patches in its `LLAMA_PATCHES` order, builds both in the builder image and
writes a receipt, and `package-standard.py --llama-opencl-build BUILD` refuses
a build whose patches, recipe or outputs differ from it. libllama is installed
over the SDK's (its one patch acts only for an application that asked for the
native CPU fallback on the Shield device). The executor is still supplied as a
file; the package records the digest of `q8_executor.cpp` beside it, and it
must be linked with `-pthread`. A frontend needs `LDLIBS=-lcrypto` with or
without a table. This is an incremental packaging recipe;
its input binary hashes and source build receipts remain required provenance.
The frontend build must pass:

```
-DSHIELD_Q8_CAPTURE -DSHIELD_Q8_MASKED
-DSHIELD_COMPILER=\"/opt/shield/opencl/q8_capture_compiler.py\"
-DSHIELD_Q8_CAPTURE_LOG=\"\"
```

`test_standard_package.py` exercises installed artifacts with a desktop UID
1000 and sandboxed service UID 1001, not mounted development libraries. It
checks actual masked work, SD output against the direct SDK fixture, both
loopback identities, native service compatibility, and optional corrupt or
disconnect fault injection. Host networking is only for the dedicated test
worker; this test does not claim to validate VM firewall rules or attestation.
The separate firewall test uses synthetic local data and no network access.

`host/build.py` streams and hashes the OCI export during extraction, avoiding a
second rootfs archive. `--link-model-volume` can reuse the immutable public
model image on the same filesystem. Existing differing volumes are refused,
never overwritten. Four focused tests verify stream hashing/extraction,
corrupt-stream rejection and safe volume reuse; existing release digest checks
still apply. A separate measured image and actual CVM tests are required before
promotion. General CUDA binary compatibility, broader numerical/performance
acceptance, protected graphics and publication remain unfinished.

Broader host diagnostics now cover four eight-step SD images and four
teacher-forced Qwen texts. Both standard and native masked paths agree exactly
on these fixtures; original CPU arithmetic differs. See `STANDARD_NUMERICS.md`
for measurements and the placement investigation. The finite validator was
deployed and retested in 0.6.10. Compiler-rejection hints reduce startup work
in host comparisons; the release procedure and its enforced binding checks
are documented in `../opencl/SOURCE_DECLINES.md`. They never authorize offload.
