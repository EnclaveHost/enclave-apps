# Packaged standard GPU applications

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
`llama-shield-source-cache.patch`. This is an incremental packaging recipe;
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
